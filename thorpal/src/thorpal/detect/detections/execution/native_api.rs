//! Native API / direct-syscall abuse (MITRE ATT&CK **T1106**).
//!
//! Detects malware that reaches the kernel through the *lowest-level* interface
//! available so its behavior does not surface at the monitored Win32 layer:
//! `ntdll` `Nt*`/`Zw*` calls (native twins of Win32 file/process/memory/registry/
//! loader APIs — calling `NtWriteFile` instead of `WriteFile` dodges hooks on
//! kernel32/kernelbase), and direct `SYSCALL`/`INT 0x2E` (skipping `ntdll` so even
//! native-API hooks are evaded). Direct syscalls are flagged only in PE targets:
//! in a static ELF every libc call is a syscall, so the raw opcode carries no
//! signal there (the notable Linux case, `ptrace`, is handled by `debugger_evasion`).
//!
//! A bare gate opcode is not claimed as a direct syscall on its own: the site
//! must show its syscall-number setup (an immediate, any `eax`/`rax` write such
//! as the Hell's-Gate-style memory load, or a `MOV R10,RCX` stub prologue at a
//! function entry). Opcodes with no setup in reach — the shape VMProtect-packed
//! binaries litter across overlapping pseudo-functions — are deduplicated across
//! those overlapping roots and demoted to one weak observation, collapsing into
//! a single file-level rollup when they spread over several caller-less roots.
//! Co-occurring injection natives (the `NtCreateSection`+`NtMapViewOfSection`
//! section-mapping pair, the write/execute natives) are promoted to their full
//! grade, with each content naming its partners.
//!
//! APIs catalogued by other detections (`NtClose`, `NtQueryInformationProcess`,
//! `NtDelayExecution`) are intentionally not duplicated here.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::fmt::Write as _;

use anyhow::Result;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, InsnCtx, api_call_hits,
    group_scan_hits, high, low, medium, reg_value_before, scan_instructions,
};
use crate::thorpal::detect::op_str;

use super::T1106;

/// The Native API detection (ATT&CK T1106).
pub struct NativeApi;

/// Native `ntdll` `Nt*`/`Zw*` functions and why each matters.
///
/// Each has a documented Win32 equivalent; calling the native form is the tell
/// that the malware is avoiding the Win32 hook layer. Injection primitives are
/// rated `high`; ordinary file/registry/loader natives are `medium`.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "NtCreateFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtCreateFile instead of Win32 CreateFile; the ntdll form bypasses \
                    user-mode hooks on kernel32/kernelbase, so file activity is not seen at the \
                    monitored Win32 layer.",
    },
    ApiSpec {
        key: "NtOpenFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtOpenFile instead of CreateFile/OpenFile to open a handle below \
                    the monitored Win32 layer.",
    },
    ApiSpec {
        key: "NtReadFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtReadFile instead of Win32 ReadFile; reading files via ntdll evades \
                    hooks on the Win32 file APIs, a common ransomware and stealer pattern.",
    },
    ApiSpec {
        key: "NtWriteFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtWriteFile instead of Win32 WriteFile; writing files via ntdll \
                    evades hooks on the Win32 file APIs, a common ransomware and wiper pattern.",
    },
    ApiSpec {
        key: "NtQueryDirectoryFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtQueryDirectoryFile instead of FindFirstFile/FindNextFile to \
                    enumerate a directory without the monitored Win32 enumeration APIs.",
    },
    ApiSpec {
        key: "NtSetInformationFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtSetInformationFile instead of the Win32 file APIs — depending on \
                    the FileInformationClass argument this renames (10), sets delete-on-close (13), \
                    or rewrites basic info/timestamps (4, timestomping); recover the class to tell \
                    which.",
    },
    ApiSpec {
        key: "NtOpenProcess",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtOpenProcess instead of OpenProcess to obtain a process handle \
                    below the Win32 layer, typically a precursor to native-API injection.",
    },
    ApiSpec {
        key: "NtAllocateVirtualMemory",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtAllocateVirtualMemory instead of VirtualAlloc(Ex); allocating \
                    (often RWX) memory via ntdll evades Win32 allocation hooks — a classic injection \
                    building block.",
    },
    ApiSpec {
        key: "NtProtectVirtualMemory",
        always_noise: false,
        severity: high(),
        reasoning: "Calls native NtProtectVirtualMemory instead of VirtualProtect(Ex) to change page \
                    protections (e.g. make a buffer executable) below the Win32 hook layer — a strong \
                    injection/self-modifying-code tell.",
    },
    ApiSpec {
        key: "NtWriteVirtualMemory",
        always_noise: false,
        severity: high(),
        reasoning: "Calls native NtWriteVirtualMemory instead of WriteProcessMemory to write into \
                    another process via ntdll — the core primitive of native-API process injection.",
    },
    ApiSpec {
        key: "NtMapViewOfSection",
        always_noise: false,
        severity: high(),
        reasoning: "Calls native NtMapViewOfSection to map a section into a (remote) process — the \
                    section-mapping injection technique that avoids WriteProcessMemory entirely.",
    },
    ApiSpec {
        key: "NtCreateThreadEx",
        always_noise: false,
        severity: high(),
        reasoning: "Calls native NtCreateThreadEx instead of CreateRemoteThread to start execution in \
                    a (remote) process via ntdll — a hallmark of native-API code injection.",
    },
    ApiSpec {
        key: "NtQueueApcThread",
        always_noise: false,
        severity: high(),
        reasoning: "Calls native NtQueueApcThread to queue an APC into a thread — the APC-injection \
                    execution primitive, performed below the Win32 layer.",
    },
    ApiSpec {
        key: "NtCreateSection",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtCreateSection to create a memory section, commonly paired with \
                    NtMapViewOfSection for section-mapping process injection.",
    },
    ApiSpec {
        key: "NtResumeThread",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtResumeThread to resume a (suspended, possibly hollowed) thread via \
                    ntdll — the trigger step of process hollowing / injection.",
    },
    ApiSpec {
        key: "NtCreateKey",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtCreateKey instead of RegCreateKey(Ex) to create a registry key \
                    below the monitored Win32 registry APIs (e.g. for stealthy persistence).",
    },
    ApiSpec {
        key: "NtSetValueKey",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtSetValueKey instead of RegSetValue(Ex) to write a registry value \
                    without the monitored Win32 registry APIs (e.g. a Run-key persistence value).",
    },
    ApiSpec {
        key: "NtQueryValueKey",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls native NtQueryValueKey instead of RegQueryValue(Ex) to read the registry \
                    below the Win32 layer.",
    },
    ApiSpec {
        key: "LdrLoadDll",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls ntdll LdrLoadDll directly instead of LoadLibrary; manual module loading via \
                    the loader API bypasses hooks on LoadLibrary and hides which DLLs are pulled in.",
    },
    ApiSpec {
        key: "LdrGetProcedureAddress",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls ntdll LdrGetProcedureAddress instead of GetProcAddress to resolve API \
                    addresses below the Win32 layer — dynamic API resolution that hides the IAT.",
    },
];

/// The section-mapping injection pair whose co-occurrence earns the full grade.
const SECTION_PAIR: &[&str] = &["NtCreateSection", "NtMapViewOfSection"];

/// The write/execute injection natives whose co-occurrence earns the full grade.
const WRITE_EXECUTE_NATIVES: &[&str] = &[
    "NtWriteVirtualMemory",
    "NtCreateThreadEx",
    "NtQueueApcThread",
];

/// How many instructions before a gate opcode are searched for its
/// syscall-number setup (the same window the sibling `debugger_evasion` gate
/// uses for its `ptrace` check).
const SSN_WINDOW: usize = 8;

/// Mnemonics counted as writing their first operand, for the mandatory
/// any-`eax`-write fallback of the syscall-number gate.
const EAX_WRITE_MNEMONICS: &[&str] = &["MOV", "XOR", "LEA", "ADD", "OR"];

/// How close to its function entry a gate opcode must sit for the
/// `MOV R10,RCX` stub-prologue shape to count as its setup evidence.
const STUB_PROLOGUE_SPAN: usize = 4;

/// The most function roots allowed to carry only bare (setup-less) gate
/// opcodes before their per-function observations collapse into one
/// file-level rollup.
const MAX_BARE_SYSCALL_FUNCTIONS: usize = 2;

/// The most addresses spelled out in a rollup/overlap listing before the rest
/// are counted instead.
const MAX_ROLLUP_LISTED: usize = 12;

/// How a `SYSCALL`/`INT 0x2E` site's syscall-number setup was evidenced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SsnEvidence {
    /// An immediate syscall number was recovered from the preceding window.
    Immediate(u64),
    /// A non-immediate `eax`/`rax` write (e.g. the Hell's-Gate-style
    /// `MOV EAX,[g_ssn]` memory load) precedes the gate; carries the staging
    /// instruction's rendered text for the reasoning.
    RegisterWrite(String),
    /// The gate sits just behind a `MOV R10,RCX` syscall-stub prologue at its
    /// function entry, so the syscall number is staged by the caller.
    StubPrologue,
    /// No syscall-number setup is in reach: an unproven bare opcode.
    Unproven,
}

/// Rank setup evidence so cross-root merges keep the strongest form.
///
/// # Arguments
///
/// * `evidence` - The evidence to rank
fn evidence_rank(evidence: &SsnEvidence) -> u8 {
    // immediates prove the exact number, register writes prove staging, a stub
    // prologue proves the shape, and a bare opcode proves nothing
    match evidence {
        SsnEvidence::Immediate(_) => 3,
        SsnEvidence::RegisterWrite(_) => 2,
        SsnEvidence::StubPrologue => 1,
        SsnEvidence::Unproven => 0,
    }
}

/// One raw `SYSCALL`/`INT 0x2E` site collected by the instruction scan.
#[derive(Debug)]
struct SyscallSite {
    /// The gate instruction's address.
    addr: u64,
    /// The Ghidra function root the site was decoded under.
    root: u64,
    /// Whether the gate is the legacy `INT 0x2E` form rather than `SYSCALL`.
    legacy: bool,
    /// The syscall-number setup evidence found around the site.
    evidence: SsnEvidence,
}

/// One gate site after deduplication across overlapping function roots.
#[derive(Debug)]
struct DedupedSite {
    /// The gate instruction's address.
    addr: u64,
    /// Every function root the site decoded under, sorted ascending (packed
    /// binaries carve overlapping pseudo-functions over one byte region, so a
    /// single `0f05` can be decoded several times).
    roots: Vec<u64>,
    /// Whether the gate is the legacy `INT 0x2E` form rather than `SYSCALL`.
    legacy: bool,
    /// The strongest setup evidence seen across the overlapping decodes.
    evidence: SsnEvidence,
}

/// Classify the syscall-number setup evidence around a gate opcode.
///
/// Step 1 recovers an immediate number from the preceding window (the same
/// helper and window `debugger_evasion`'s `ptrace` gate uses). Step 2 is the
/// mandatory fallback: any `eax`/`rax`-writing instruction in the window
/// counts, because the confirmed Hell's Gate stub stages its number with
/// `MOV EAX,[g_ssn]` — a bracketed memory operand the immediate parser cannot
/// read. Step 3 accepts a `MOV R10,RCX` stub prologue right at the function
/// entry (a caller-staged stub). Anything else is an unproven bare opcode.
///
/// # Arguments
///
/// * `cursor` - The scan cursor positioned on the gate instruction
fn classify_ssn_evidence(cursor: &InsnCtx) -> SsnEvidence {
    // step 1: an immediate (or xor-zeroed) syscall number staged in the window
    if let Some(ssn) = reg_value_before(cursor.preceding(SSN_WINDOW), "eax", "rax") {
        return SsnEvidence::Immediate(ssn);
    }
    // step 2 (mandatory fallback): the most recent eax/rax write in the window,
    // whatever its source operand looks like
    for insn in cursor.preceding(SSN_WINDOW).iter().rev() {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        let ops = op_str(insn).to_ascii_lowercase();
        // the written register is the first operand of these mnemonics
        let first = ops.split(',').next().unwrap_or("").trim().to_string();
        if EAX_WRITE_MNEMONICS.contains(&mnemonic.as_str()) && (first == "eax" || first == "rax") {
            return SsnEvidence::RegisterWrite(format!("{} {}", insn.mnemonic, op_str(insn)));
        }
    }
    // step 3: a function-entry-aligned `MOV R10,RCX` prologue marks a
    // caller-staged syscall stub even without a local eax write
    if cursor.idx <= STUB_PROLOGUE_SPAN {
        let prologue = cursor.preceding(cursor.idx).iter().any(|insn| {
            insn.mnemonic.eq_ignore_ascii_case("mov")
                && op_str(insn).to_ascii_lowercase().replace(' ', "") == "r10,rcx"
        });
        if prologue {
            return SsnEvidence::StubPrologue;
        }
    }
    // nothing in reach stages the number: an unproven gadget, not a stub
    SsnEvidence::Unproven
}

/// Deduplicate raw gate sites by address, merging overlapping function roots.
///
/// Packed binaries make Ghidra carve several pseudo-functions over one byte
/// region, so the same gate decodes under multiple roots (and each decode may
/// see a different preceding window); one record per address survives, keeping
/// every root and the strongest evidence seen.
///
/// # Arguments
///
/// * `raw` - The raw sites collected by the instruction scan
fn dedupe_sites(raw: Vec<SyscallSite>) -> Vec<DedupedSite> {
    // bucket by gate address; the BTreeMap keeps the output address-ordered
    let mut map: BTreeMap<u64, DedupedSite> = BTreeMap::new();
    for site in raw {
        match map.entry(site.addr) {
            Entry::Occupied(mut slot) => {
                let merged = slot.get_mut();
                // record the additional root this address decoded under
                if !merged.roots.contains(&site.root) {
                    merged.roots.push(site.root);
                }
                // keep the strongest setup evidence across the decodes
                if evidence_rank(&site.evidence) > evidence_rank(&merged.evidence) {
                    merged.evidence = site.evidence;
                }
            }
            Entry::Vacant(slot) => {
                slot.insert(DedupedSite {
                    addr: site.addr,
                    roots: vec![site.root],
                    legacy: site.legacy,
                    evidence: site.evidence,
                });
            }
        }
    }
    // sort each root list so the lowest root is the stable primary
    let mut sites: Vec<DedupedSite> = map.into_values().collect();
    for site in &mut sites {
        site.roots.sort_unstable();
    }
    sites
}

/// Render an address list, spelling out the first few and counting the rest.
///
/// # Arguments
///
/// * `addrs` - The addresses to render, already in the wanted order
fn render_addr_list(addrs: &[u64]) -> String {
    // spell out the first few addresses
    let listed = addrs
        .iter()
        .take(MAX_ROLLUP_LISTED)
        .map(|addr| fmt_addr(*addr))
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: count whatever was left out
    if addrs.len() > MAX_ROLLUP_LISTED {
        format!("{listed} (+{} more)", addrs.len() - MAX_ROLLUP_LISTED)
    } else {
        listed
    }
}

/// The reasoning note naming a site's overlapping function roots, when several.
///
/// # Arguments
///
/// * `roots` - The site's sorted function roots
fn multi_root_note(roots: &[u64]) -> String {
    // a single root needs no note
    if roots.len() <= 1 {
        return String::new();
    }
    format!(
        " This byte region decodes under {} overlapping function roots ({}) — Ghidra \
         pseudo-functions carved over one region — so the site is reported once.",
        roots.len(),
        render_addr_list(roots)
    )
}

/// Build the full-strength hit for one gate site with proven number setup.
///
/// # Arguments
///
/// * `site` - The deduplicated gate site (evidence is never `Unproven` here)
fn gated_hit(site: &DedupedSite) -> DetectionHit {
    // label the gate form (modern SYSCALL vs the legacy interrupt)
    let label = if site.legacy {
        "INT 0x2E system-call gate"
    } else {
        "direct SYSCALL"
    };
    // a recovered immediate renders in the content so the lead is
    // self-corroborating; other setup forms keep the plain shape
    let content = match site.evidence {
        SsnEvidence::Immediate(ssn) => format!("{label} (EAX={ssn:#x}) @ {}", fmt_addr(site.addr)),
        _ => format!("{label} @ {}", fmt_addr(site.addr)),
    };
    // the base claim per gate form
    let base = if site.legacy {
        "Enters the kernel via the legacy INT 0x2E system-call gate instead of calling \
         ntdll, bypassing user-mode API/native hooks."
    } else {
        "Issues a SYSCALL directly instead of calling ntdll. This 'direct syscall' \
         technique skips the ntdll stubs so even native-API hooks are bypassed, entering \
         the kernel with no monitored user-mode frame."
    };
    // say which setup evidence proved the stub shape
    let evidence = match &site.evidence {
        SsnEvidence::Immediate(ssn) => format!(
            " The syscall number is staged as an immediate (EAX={ssn:#x}) within reach of \
             the gate, so the site is a self-contained system-call stub."
        ),
        SsnEvidence::RegisterWrite(staging) => format!(
            " The syscall-number register is loaded from a non-immediate source just \
             before the gate (`{staging}`) — the stub shape used when syscall numbers are \
             resolved or harvested at runtime."
        ),
        SsnEvidence::StubPrologue => " The gate sits at a function entry behind the `MOV R10,RCX` \
                                      syscall-stub prologue, with the syscall number staged by the \
                                      caller."
            .to_string(),
        SsnEvidence::Unproven => String::new(),
    };
    // assemble the per-site hit at its full (medium) per-site strength
    DetectionHit::new(
        T1106,
        site.addr,
        site.roots.first().copied(),
        medium(),
        content,
        format!("{base}{evidence}{}", multi_root_note(&site.roots)),
    )
    .kind(EvidenceKind::ControlFlowGated)
}

/// Build the demoted hit for one bare gate site with no setup in reach.
///
/// # Arguments
///
/// * `site` - The deduplicated bare gate site
fn bare_hit(site: &DedupedSite) -> DetectionHit {
    // label the gate form so the hypothesis names what actually decodes here
    let (label, gate) = if site.legacy {
        ("bare INT 0x2E opcode", "INT 0x2E")
    } else {
        ("bare SYSCALL opcode", "SYSCALL")
    };
    let content = format!(
        "{label} (no syscall-number setup in reach) @ {}",
        fmt_addr(site.addr)
    );
    // hypothesis form: the opcode is recorded, the direct-syscall claim is not
    let reasoning = format!(
        "A {gate} instruction decodes here, but nothing in reach stages the syscall \
         number (no immediate, no eax/rax write, no stub prologue), so this is not a \
         provable system-call stub. Bare gate opcodes like this are commonly protector \
         output (VM-exit gadgets, mutation junk between packed-call trampolines) or stray \
         decodes, so the opcode is recorded as an unproven observation only.{}",
        multi_root_note(&site.roots)
    );
    // demoted: a weak, context-only observation rather than an unsure finding
    DetectionHit::new(
        T1106,
        site.addr,
        site.roots.first().copied(),
        low(),
        content,
        reasoning,
    )
}

/// Build the one file-level rollup for bare gate opcodes spread across many
/// caller-less roots.
///
/// # Arguments
///
/// * `sites` - The bare sites being rolled up
/// * `roots` - The distinct function roots those sites decode under
fn bare_rollup_hit(sites: &[DedupedSite], roots: &BTreeSet<u64>) -> DetectionHit {
    // name the gate forms present so a mixed cluster is labeled honestly
    let any_syscall = sites.iter().any(|site| !site.legacy);
    let any_legacy = sites.iter().any(|site| site.legacy);
    let label = match (any_syscall, any_legacy) {
        (true, true) => "direct SYSCALL/INT 0x2E opcode",
        (false, true) => "INT 0x2E opcode",
        _ => "direct SYSCALL opcode",
    };
    // every deduplicated gate address rides along, sorted for stable rendering
    let mut addrs: Vec<u64> = sites.iter().map(|site| site.addr).collect();
    addrs.sort_unstable();
    let content = format!(
        "{label} in {} functions — sites @ {}",
        roots.len(),
        render_addr_list(&addrs)
    );
    let root_list: Vec<u64> = roots.iter().copied().collect();
    // one hypothesis-form observation carrying every root and site
    let reasoning = format!(
        "Bare system-call gate opcodes decode at {} sites across {} function roots ({}), \
         none with a recoverable syscall-number setup (immediate, eax/rax write, or stub \
         prologue) in reach. Overlapping, caller-less function ranges over such bytes are \
         the shape VMProtect-style protectors leave — VM-exit gadgets and mutation junk \
         between packed-call trampolines — so this is recorded as a single unproven \
         observation instead of per-function direct-syscall findings. If these gates \
         execute at all, their syscall numbers are staged dynamically (e.g. by a \
         protector VM), which only deeper analysis can prove.",
        addrs.len(),
        roots.len(),
        render_addr_list(&root_list)
    );
    // file-level: no single pseudo-function owns the observation
    DetectionHit::new(T1106, 0, None, low(), content, reasoning).sites(addrs)
}

/// Turn the scan's raw gate sites into deduplicated, grouped detection hits.
///
/// Sites are deduplicated by address across overlapping roots, split by their
/// setup evidence, grouped per containing function (via the shared
/// [`group_scan_hits`] merge), and — when bare opcodes spread over more than
/// [`MAX_BARE_SYSCALL_FUNCTIONS`] roots that carry no proven stub — collapsed
/// into one file-level rollup instead of per-function posts.
///
/// # Arguments
///
/// * `raw` - The raw sites collected by the instruction scan
fn build_syscall_hits(raw: Vec<SyscallSite>) -> Vec<DetectionHit> {
    // merge the overlapping-root duplicates first
    let deduped = dedupe_sites(raw);
    // split proven stubs from bare opcodes
    let (gated, bare): (Vec<DedupedSite>, Vec<DedupedSite>) = deduped
        .into_iter()
        .partition(|site| !matches!(site.evidence, SsnEvidence::Unproven));
    // roots carrying at least one proven stub keep their bare siblings local
    let gated_roots: BTreeSet<u64> = gated
        .iter()
        .flat_map(|site| site.roots.iter().copied())
        .collect();
    // the proven stubs seed at full per-site strength
    let mut per_site: Vec<DetectionHit> = gated.iter().map(gated_hit).collect();
    // bare sites whose every root is caller-less feed the rollup; the rest stay
    // local to corroborate the proven stub in their function
    let (rollup, local): (Vec<DedupedSite>, Vec<DedupedSite>) = bare
        .into_iter()
        .partition(|site| site.roots.iter().all(|root| !gated_roots.contains(root)));
    // count the distinct roots carrying only bare opcodes
    let rollup_roots: BTreeSet<u64> = rollup
        .iter()
        .flat_map(|site| site.roots.iter().copied())
        .collect();
    // past the threshold the bare mass becomes one file-level observation;
    // under it every bare site still seeds (demoted) per function
    let rolled = rollup_roots.len() > MAX_BARE_SYSCALL_FUNCTIONS;
    if !rolled {
        per_site.extend(rollup.iter().map(bare_hit));
    }
    per_site.extend(local.iter().map(bare_hit));
    // merge same-shape sites per containing function (the cap pass is unused:
    // the rollup above already applies this detector's cross-function collapse)
    let mut hits = group_scan_hits(per_site, 0, |_| false);
    // append the file-level rollup after the per-function findings
    if rolled {
        hits.push(bare_rollup_hit(&rollup, &rollup_roots));
    }
    hits
}

/// Promote co-occurring injection natives to their full catalog grade.
///
/// Presence of one injection native is a context-dependent claim, but the
/// *combination* is the real signal: when both section-mapping natives
/// (`NtCreateSection` + `NtMapViewOfSection`) resolve call sites in the same
/// program, or two of the write/execute natives do, each paired hit is raised
/// to [`high`] and its content names its partners — the seeder derives its
/// injection-chain question from exactly that paired content shape.
///
/// # Arguments
///
/// * `hits` - The catalog hits from [`api_call_hits`], adjusted in place
fn promote_cooccurring_natives(hits: &mut [DetectionHit]) {
    // the section-mapping pair: create a section, then map a view of it
    promote_cooccurrence(
        hits,
        SECTION_PAIR,
        "the section-mapping process-injection pair (create a section, then map a view \
         of it into a target process)",
    );
    // the write/execute natives: write into a process and start it running
    promote_cooccurrence(
        hits,
        WRITE_EXECUTE_NATIVES,
        "a native-API injection chain (writing into a process and/or starting code \
         running in it below the Win32 layer)",
    );
}

/// Promote one co-occurrence set's hits when two or more of its APIs resolved
/// call sites.
///
/// # Arguments
///
/// * `hits` - The catalog hits to adjust in place
/// * `apis` - The co-occurrence set's normalized API names
/// * `label` - The analyst-facing name of what the combination evidences
fn promote_cooccurrence(hits: &mut [DetectionHit], apis: &[&str], label: &str) {
    // which of the set's APIs actually resolved call sites in this program
    // (census and imported-only hits carry no `api` stamp, so they never count)
    let present: Vec<&str> = apis
        .iter()
        .copied()
        .filter(|api| hits.iter().any(|hit| hit.api == Some(*api)))
        .collect();
    // the promotion is about the combination; one API alone stays as seeded
    if present.len() < 2 {
        return;
    }
    for hit in hits.iter_mut() {
        // only the set's own call-site hits are promoted
        let Some(api) = hit.api else {
            continue;
        };
        if !apis.contains(&api) {
            continue;
        }
        // name the partners in the content so the pairing survives into the
        // forum post (and the seeder's derived question keys on it)
        let others: Vec<&str> = present
            .iter()
            .copied()
            .filter(|other| *other != api)
            .collect();
        let _ = write!(hit.content, " (co-occurs with {})", others.join(", "));
        let _ = write!(
            hit.reasoning,
            " {api} co-occurs with {} in this program — {label}. The combination, not any \
             single call, is what raises this hit to its full grade.",
            others.join(" and ")
        );
        // the pairing earns the full catalog grade now; any clamp-withheld
        // severity is applied rather than left pending for the validator
        let (confidence, suspicion) = high();
        hit.confidence = confidence;
        hit.suspicion = suspicion;
        hit.catalog_severity = None;
    }
}

/// Return whether the analyzed program is a PE (Windows) image.
///
/// The direct-syscall opcode is only a signal on Windows; on a statically-linked
/// ELF every libc call is a syscall, so the raw opcode there carries no meaning.
///
/// # Arguments
///
/// * `ctx` - The analysis context (its program metadata carries the format)
fn is_pe(ctx: &AnalysisContext) -> bool {
    // treat any "PE"/"Portable Executable" executable format as a Windows target
    ctx.program.executable_format.as_deref().is_some_and(|f| {
        let f = f.to_ascii_uppercase();
        f.contains("PE") || f.contains("PORTABLE")
    })
}

#[async_trait::async_trait]
impl Detection for NativeApi {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "native_api"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1106
    }

    /// Flag native-API call sites and (PE only) setup-evidenced direct system
    /// calls.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context
    #[tracing::instrument(name = "NativeApi::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();
        // 1) native ntdll Nt*/Zw* API call sites (their result gates behavior)
        hits.extend(api_call_hits(ctx, T1106, APIS, EvidenceKind::ApiResultCheck).await?);
        // 2) promote the co-occurring injection natives (the section-mapping
        //    pair and the write/execute natives) to their full grade
        promote_cooccurring_natives(&mut hits);
        // 3) direct SYSCALL / INT 0x2E — only meaningful on PE targets (see
        //    docs); the closure only records sites, because hits are built
        //    after cross-root dedup, setup gating, grouping, and rollup
        if is_pe(ctx) {
            let mut sites: Vec<SyscallSite> = Vec::new();
            scan_instructions(ctx, |c| {
                let m = c.mnemonic();
                let ops = c.operands().replace(' ', "");
                // a hand-rolled system call, skipping ntdll's stubs entirely
                if m == "SYSCALL" {
                    sites.push(SyscallSite {
                        addr: c.addr(),
                        root: c.func_entry,
                        legacy: false,
                        evidence: classify_ssn_evidence(c),
                    });
                }
                // INT 0x2E — the legacy system-call gate, same intent (and the
                // identical syscall-number gate) as SYSCALL
                if m == "INT" && (ops == "0x2e" || ops.ends_with(",0x2e")) {
                    sites.push(SyscallSite {
                        addr: c.addr(),
                        root: c.func_entry,
                        legacy: true,
                        evidence: classify_ssn_evidence(c),
                    });
                }
                None
            })
            .await?;
            hits.extend(build_syscall_hits(sites));
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::confidence::Confidence;
    use ghidra_cli::models::Instruction;

    /// Build a minimal instruction fixture.
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

    /// Build a raw scan site fixture.
    ///
    /// # Arguments
    ///
    /// * `addr` - The gate instruction's address
    /// * `root` - The function root the site decoded under
    /// * `legacy` - Whether the gate is the legacy `INT 0x2E` form
    /// * `evidence` - The setup evidence found around the site
    fn site(addr: u64, root: u64, legacy: bool, evidence: SsnEvidence) -> SyscallSite {
        SyscallSite {
            addr,
            root,
            legacy,
            evidence,
        }
    }

    /// Build a catalog-style call-site hit stamped with its API name.
    ///
    /// # Arguments
    ///
    /// * `api` - The normalized catalog API name
    /// * `function` - The calling function's entry address
    /// * `addr` - The call site address
    fn api_hit(api: &'static str, function: u64, addr: u64) -> DetectionHit {
        DetectionHit::new(
            T1106,
            addr,
            Some(function),
            medium(),
            format!("{api} call @ {}", fmt_addr(addr)),
            format!("Calls native {api}."),
        )
        .kind(EvidenceKind::ApiResultCheck)
        .api(api)
    }

    /// The switchback Hell's Gate stub (memory-staged syscall number) still
    /// seeds at full per-site strength via the mandatory any-EAX-write fallback.
    #[test]
    fn hells_gate_memory_staged_stub_keeps_seeding() {
        // fn-180005351's shape: mov r10,rcx; mov eax,[g_ssn]; syscall; ret
        let insns = vec![
            insn("MOV", "R10,RCX", 0x1_8000_5351),
            insn("MOV", "EAX,dword ptr [DAT_18001b000]", 0x1_8000_5354),
            insn("SYSCALL", "", 0x1_8000_535a),
            insn("RET", "", 0x1_8000_535c),
        ];
        let cursor = InsnCtx {
            func_entry: 0x1_8000_5351,
            insns: &insns,
            idx: 2,
        };
        // the bracketed memory operand defeats the immediate parser, so the
        // register-write fallback is what must prove the stub
        let evidence = classify_ssn_evidence(&cursor);
        assert!(
            matches!(evidence, SsnEvidence::RegisterWrite(_)),
            "{evidence:?}"
        );
        let hits = build_syscall_hits(vec![site(0x1_8000_535a, 0x1_8000_5351, false, evidence)]);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        // full per-site strength: deep-confirmed in every session it appeared
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, medium().1);
        assert_eq!(hit.content, "direct SYSCALL @ 0x18000535a");
        assert_eq!(hit.function, Some(0x1_8000_5351));
        assert_eq!(hit.kind, EvidenceKind::ControlFlowGated);
        // the staging instruction is named so the lead is self-corroborating
        assert!(
            hit.reasoning.contains("MOV EAX,dword ptr [DAT_18001b000]"),
            "{}",
            hit.reasoning
        );
    }

    /// An immediate syscall number is recovered and rendered in the content.
    #[test]
    fn immediate_ssn_renders_in_content() {
        let insns = vec![
            insn("MOV", "EAX,0x3c", 0x40_1000),
            insn("SYSCALL", "", 0x40_1005),
        ];
        let cursor = InsnCtx {
            func_entry: 0x40_1000,
            insns: &insns,
            idx: 1,
        };
        let evidence = classify_ssn_evidence(&cursor);
        assert_eq!(evidence, SsnEvidence::Immediate(0x3c));
        let hits = build_syscall_hits(vec![site(0x40_1005, 0x40_1000, false, evidence)]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "direct SYSCALL (EAX=0x3c) @ 0x401005");
        assert!(matches!(hits[0].confidence, Confidence::Unsure));
    }

    /// A caller-staged stub (`MOV R10,RCX` prologue, no local EAX write) still
    /// passes the gate through the function-entry stub-prologue shape.
    #[test]
    fn entry_aligned_stub_prologue_passes_gate() {
        let insns = vec![
            insn("MOV", "R10,RCX", 0x40_2000),
            insn("SYSCALL", "", 0x40_2003),
            insn("RET", "", 0x40_2005),
        ];
        let cursor = InsnCtx {
            func_entry: 0x40_2000,
            insns: &insns,
            idx: 1,
        };
        assert_eq!(classify_ssn_evidence(&cursor), SsnEvidence::StubPrologue);
    }

    /// A bare `0f05` inside VMP mutation junk (no EAX write in reach) is
    /// classified as unproven, not as a direct-syscall stub.
    #[test]
    fn vmp_mutation_junk_window_classifies_unproven() {
        // the KONNI s4 shape: bare SYSCALL gadgets embedded in VMP mutation
        // junk with no MOV EAX,<SSN> anywhere in reach
        let insns = vec![
            insn("PUSH", "0x12345678", 0x1_808f_e770),
            insn("CALL", "0x1808f0000", 0x1_808f_e775),
            insn("XCHG", "ESI,EDI", 0x1_808f_e77a),
            insn("LEA", "RDX,[RSP + 0x20]", 0x1_808f_e77c),
            insn("ADD", "RSI,0x8", 0x1_808f_e77e),
            insn("SYSCALL", "", 0x1_808f_e780),
        ];
        let cursor = InsnCtx {
            func_entry: 0x1_808f_e770,
            insns: &insns,
            idx: 5,
        };
        assert_eq!(classify_ssn_evidence(&cursor), SsnEvidence::Unproven);
    }

    /// keentroll's overlapping VMP pseudo-functions collapse to one file-level
    /// rollup carrying every root and deduplicated site.
    #[test]
    fn keentroll_vmp_cluster_rolls_up_to_one_file_level_hit() {
        // the real shape: 0f05 bytes @0x235fd5379/92/e8/41 decoded under four
        // overlapping roots (FUN_235fd5392 starts 0x19 bytes inside
        // FUN_235fd5379), each decode seeing no SSN setup
        let raw = vec![
            site(0x2_35fd_5341, 0x2_35fd_5341, false, SsnEvidence::Unproven),
            site(0x2_35fd_5379, 0x2_35fd_5341, false, SsnEvidence::Unproven),
            site(0x2_35fd_5379, 0x2_35fd_5379, false, SsnEvidence::Unproven),
            site(0x2_35fd_5392, 0x2_35fd_5379, false, SsnEvidence::Unproven),
            site(0x2_35fd_5392, 0x2_35fd_5392, false, SsnEvidence::Unproven),
            site(0x2_35fd_53e8, 0x2_35fd_53dd, false, SsnEvidence::Unproven),
        ];
        let hits = build_syscall_hits(raw);
        // one lead instead of four near-identical per-root posts
        assert_eq!(hits.len(), 1, "{hits:#?}");
        let hit = &hits[0];
        // file-level and demoted: an unproven observation, not unsure/40 posts
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, low().1);
        assert_eq!(hit.function, None);
        assert_eq!(hit.address, 0);
        assert_eq!(hit.kind, EvidenceKind::ContextOnly);
        // the content names every root and deduplicated site once
        assert_eq!(
            hit.content,
            "direct SYSCALL opcode in 4 functions — sites @ 0x235fd5341, 0x235fd5379, \
             0x235fd5392, 0x235fd53e8"
        );
        assert_eq!(
            hit.sites,
            vec![0x2_35fd_5341, 0x2_35fd_5379, 0x2_35fd_5392, 0x2_35fd_53e8]
        );
        // hypothesis form: the protector attribution is said out loud
        assert!(hit.reasoning.contains("unproven"), "{}", hit.reasoning);
    }

    /// KONNI's five mutation-junk roots roll up instead of seeding five posts.
    #[test]
    fn konni_mutation_junk_roots_roll_up() {
        // the fn-1808fe780/82/9b/cb/ff family: one bare gadget per root
        let roots = [
            0x1_808f_e780_u64,
            0x1_808f_e782,
            0x1_808f_e79b,
            0x1_808f_e7cb,
            0x1_808f_e7ff,
        ];
        let raw = roots
            .iter()
            .map(|root| site(root + 0x11, *root, false, SsnEvidence::Unproven))
            .collect();
        let hits = build_syscall_hits(raw);
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert!(
            hits[0]
                .content
                .starts_with("direct SYSCALL opcode in 5 functions"),
            "{}",
            hits[0].content
        );
        assert!(matches!(hits[0].confidence, Confidence::Untrusted));
        assert_eq!(hits[0].sites.len(), 5);
    }

    /// Up to two bare-opcode functions keep their demoted per-function hits
    /// instead of rolling up.
    #[test]
    fn few_bare_functions_stay_local_demoted_hits() {
        let raw = vec![
            site(0x40_1100, 0x40_1000, false, SsnEvidence::Unproven),
            site(0x40_2200, 0x40_2000, false, SsnEvidence::Unproven),
        ];
        let hits = build_syscall_hits(raw);
        assert_eq!(hits.len(), 2, "{hits:#?}");
        for hit in &hits {
            // demoted to a weak observation, never an unsure/40 per-site post
            assert!(matches!(hit.confidence, Confidence::Untrusted));
            assert_eq!(hit.suspicion, low().1);
            assert!(
                hit.content
                    .starts_with("bare SYSCALL opcode (no syscall-number setup in reach) @ "),
                "{}",
                hit.content
            );
            assert!(hit.function.is_some());
        }
    }

    /// Overlapping decodes of one site keep the strongest setup evidence and
    /// note the pseudo-function roots.
    #[test]
    fn overlapping_roots_dedupe_to_strongest_evidence() {
        let raw = vec![
            site(0x60_0010, 0x60_0000, false, SsnEvidence::Unproven),
            site(0x60_0010, 0x60_0008, false, SsnEvidence::Immediate(0x18)),
        ];
        let hits = build_syscall_hits(raw);
        // one hit for the one byte region, not one per overlapping root
        assert_eq!(hits.len(), 1, "{hits:#?}");
        let hit = &hits[0];
        assert_eq!(hit.content, "direct SYSCALL (EAX=0x18) @ 0x600010");
        // the lowest root is the stable primary
        assert_eq!(hit.function, Some(0x60_0000));
        assert!(hit.reasoning.contains("overlapping"), "{}", hit.reasoning);
    }

    /// Bare opcodes sharing a function with a proven stub stay out of the
    /// rollup, which still fires for the caller-less roots.
    #[test]
    fn bare_opcodes_in_gated_functions_stay_local() {
        let raw = vec![
            // a proven stub and a bare sibling in the same function
            site(0x50_0010, 0x4f_f000, false, SsnEvidence::Immediate(0x3c)),
            site(0x50_0020, 0x4f_f000, false, SsnEvidence::Unproven),
            // three caller-less roots carrying only bare opcodes
            site(0x61_0000, 0x61_0000, false, SsnEvidence::Unproven),
            site(0x62_0000, 0x62_0000, false, SsnEvidence::Unproven),
            site(0x63_0000, 0x63_0000, false, SsnEvidence::Unproven),
        ];
        let hits = build_syscall_hits(raw);
        // one proven stub + one local bare observation + one rollup
        assert_eq!(hits.len(), 3, "{hits:#?}");
        assert_eq!(hits[0].content, "direct SYSCALL (EAX=0x3c) @ 0x500010");
        assert!(
            hits[1].content.starts_with("bare SYSCALL opcode"),
            "{}",
            hits[1].content
        );
        assert_eq!(hits[1].function, Some(0x4f_f000));
        assert!(
            hits[2]
                .content
                .starts_with("direct SYSCALL opcode in 3 functions"),
            "{}",
            hits[2].content
        );
    }

    /// Repeated gated sites in one function merge into one site-listed hit.
    #[test]
    fn gated_sites_group_per_function() {
        let raw = vec![
            site(0x40_1005, 0x40_1000, false, SsnEvidence::Immediate(0x3c)),
            site(0x40_1045, 0x40_1000, false, SsnEvidence::Immediate(0x3c)),
        ];
        let hits = build_syscall_hits(raw);
        assert_eq!(hits.len(), 1, "{hits:#?}");
        // one behavior repeated is one finding carrying both sites
        assert_eq!(
            hits[0].content,
            "direct SYSCALL (EAX=0x3c) — 2 sites @ 0x401005, 0x401045"
        );
        assert_eq!(hits[0].sites, vec![0x40_1005, 0x40_1045]);
    }

    /// The legacy `INT 0x2E` gate follows the identical syscall-number gate.
    #[test]
    fn int_2e_follows_the_same_gate() {
        let raw = vec![
            // a staged legacy gate keeps its full per-site strength
            site(0x40_3010, 0x40_3000, true, SsnEvidence::Immediate(0x25)),
            // a bare legacy gate demotes to the weak observation form
            site(0x40_4010, 0x40_4000, true, SsnEvidence::Unproven),
        ];
        let hits = build_syscall_hits(raw);
        assert_eq!(hits.len(), 2, "{hits:#?}");
        assert_eq!(
            hits[0].content,
            "INT 0x2E system-call gate (EAX=0x25) @ 0x403010"
        );
        assert!(matches!(hits[0].confidence, Confidence::Unsure));
        assert_eq!(
            hits[1].content,
            "bare INT 0x2E opcode (no syscall-number setup in reach) @ 0x404010"
        );
        assert!(matches!(hits[1].confidence, Confidence::Untrusted));
    }

    /// The section-mapping pair promotes both hits and names both APIs in each
    /// content (the shape the seeder's derived question keys on).
    #[test]
    fn section_pair_promotes_and_names_both_apis() {
        // a confirmed shape: both natives called from one function
        let mut hits = vec![
            api_hit("NtCreateSection", 0x1_8000_b5d0, 0x1_8000_b5d4),
            api_hit("NtMapViewOfSection", 0x1_8000_b5d0, 0x1_8000_b5f0).catalog_severity(high()),
        ];
        promote_cooccurring_natives(&mut hits);
        for hit in &hits {
            // both hits earn the full grade on the pairing
            assert!(matches!(hit.confidence, Confidence::Likely), "{hit:#?}");
            assert_eq!(hit.suspicion, high().1);
            // the stable contract: a section-pair lead's content carries both
            // API names, so the seeder can derive its injection question
            assert!(hit.content.contains("NtCreateSection"), "{}", hit.content);
            assert!(
                hit.content.contains("NtMapViewOfSection"),
                "{}",
                hit.content
            );
            // the applied grade is no longer pending for the validator
            assert!(hit.catalog_severity.is_none());
        }
    }

    /// A lone section native stays at its seeded grade with untouched content.
    #[test]
    fn lone_section_native_stays_as_seeded() {
        let mut hits = vec![api_hit("NtCreateSection", 0x40_1000, 0x40_1010)];
        promote_cooccurring_natives(&mut hits);
        assert!(matches!(hits[0].confidence, Confidence::Unsure));
        assert_eq!(hits[0].suspicion, medium().1);
        assert_eq!(hits[0].content, "NtCreateSection call @ 0x401010");
    }

    /// Two co-occurring write/execute natives promote while unrelated catalog
    /// hits stay untouched.
    #[test]
    fn write_execute_pair_promotes_only_its_members() {
        let mut hits = vec![
            api_hit("NtWriteVirtualMemory", 0x40_1000, 0x40_1010).catalog_severity(high()),
            api_hit("NtCreateThreadEx", 0x40_2000, 0x40_2010).catalog_severity(high()),
            api_hit("NtOpenProcess", 0x40_3000, 0x40_3010),
        ];
        promote_cooccurring_natives(&mut hits);
        // both chain members earn the full grade and name each other
        assert!(matches!(hits[0].confidence, Confidence::Likely));
        assert!(
            hits[0].content.contains("NtCreateThreadEx"),
            "{}",
            hits[0].content
        );
        assert!(matches!(hits[1].confidence, Confidence::Likely));
        assert!(
            hits[1].content.contains("NtWriteVirtualMemory"),
            "{}",
            hits[1].content
        );
        // the bystander native is not promoted or reworded
        assert!(matches!(hits[2].confidence, Confidence::Unsure));
        assert_eq!(hits[2].content, "NtOpenProcess call @ 0x403010");
    }

    /// The `NtSetInformationFile` catalog entry no longer asserts an
    /// information class it never recovers.
    #[test]
    fn nt_set_information_file_reasoning_is_class_agnostic() {
        let spec = APIS
            .iter()
            .find(|spec| spec.key == "NtSetInformationFile")
            .expect("the catalog keeps its NtSetInformationFile entry");
        // the old wording asserted rename/delete-on-close as fact
        assert!(!spec.reasoning.contains("(rename/delete-on-close)"));
        // the new wording is class-agnostic and directive
        assert!(spec.reasoning.contains("FileInformationClass"));
        assert!(spec.reasoning.contains("timestomping"));
        assert!(spec.reasoning.contains("recover the class"));
    }
}
