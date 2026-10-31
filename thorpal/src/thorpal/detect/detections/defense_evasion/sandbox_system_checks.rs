//! Virtualization/Sandbox Evasion — System Checks (MITRE ATT&CK **T1497.001**).
//!
//! Detects the ways malware fingerprints the *environment* to decide whether it
//! is running in a virtual machine or analysis sandbox (and should therefore lie
//! low or bail out):
//!
//! 1. **CPUID hypervisor probes** — `CPUID` leaf 1 (ECX bit 31 = "hypervisor
//!    present") or the hypervisor vendor leaves `0x40000000`-`0x400000FF`. Found
//!    by scanning decoded instructions and confirming the requested leaf, so
//!    ordinary CPUID feature detection (ubiquitous in crypto/SIMD code) is not
//!    flagged.
//! 2. **VM/hypervisor artifact strings** — product/driver names, device paths,
//!    VM registry keys, hypervisor MAC-address OUI prefixes, and Linux
//!    DMI/cpuinfo probes. Needle matches inside long encoded-data runs are
//!    vetoed, a defined string that is nothing but a dot-prefixed extension
//!    (`".vbox"` in a ransomware encryption-target list) is skipped, and
//!    `/proc/cpuinfo` only counts beside a hypervisor tell, because every
//!    statically linked libc carries that path via `get_nprocs()`.
//! 3. **System-fingerprint APIs** — reading running processes / hostname /
//!    username / MAC / installed drivers, reported as dual-use observations:
//!    process walking, host naming, and adapter reads build injection target
//!    lists, victim IDs, and recon reports at least as often as they feed
//!    sandbox checks, so every catalog template poses both readings and names
//!    the discriminator (a result that feeds a comparison gating control flow)
//!    instead of asserting the evasion story. The Toolhelp trio
//!    (`CreateToolhelp32Snapshot`/`Process32First`/`Process32Next`) merges into
//!    one process-walk finding per function, and the coarse survey reads (core
//!    count, RAM, disk capacity) never seed per-function findings at all —
//!    when imported and called they fold into a single file-level inventory
//!    line.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, MatchMode, MatchOptions,
    api_call_hits, confidence_rank, high, low, medium, needle_matches, normalize, reg_value_before,
    scan_instructions, string_artifact_hits_with,
};

use super::T1497_001;

/// The System Checks detection (ATT&CK T1497.001).
pub struct SandboxSystemChecks;

/// System-fingerprinting APIs whose presence is worth a dual-use observation.
///
/// Every reasoning template poses both the evasion reading and the
/// builder/recon reading and names the discriminator, because the observation
/// (the API is called) is reliable while the evasion intent is not: these
/// reads feed victim-ID builders, C2 beacons, and injection-target selection
/// at least as often as sandbox comparisons.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetComputerName",
        always_noise: false,
        severity: medium(),
        reasoning: "Reads the machine name (GetComputerName). Dual-use: sandbox checks compare it \
                    against known sandbox hostnames (SANDBOX, MALTEST); at least as often malware \
                    concatenates or ROR13/CRC32-hashes it into a victim ID embedded in a C2 \
                    beacon. Treat as evasion only if the result feeds a comparison that gates \
                    control flow.",
    },
    ApiSpec {
        key: "GetUserName",
        always_noise: false,
        severity: medium(),
        reasoning: "Reads the current account name (GetUserName). Dual-use: sandbox checks \
                    compare it against known sandbox usernames (WDAGUtilityAccount, HAPUBWS); at \
                    least as often the name is hashed into a bot/victim ID shipped in a C2 \
                    beacon. Treat as evasion only if the result feeds a comparison that gates \
                    control flow.",
    },
    ApiSpec {
        key: "GetAdaptersInfo",
        always_noise: false,
        severity: medium(),
        reasoning: "Reads adapter details including MAC addresses (GetAdaptersInfo). Dual-use: VM \
                    checks match the MAC against hypervisor OUI prefixes (VMware 00:0C:29, \
                    VirtualBox 08:00:27, ...); at least as often the values seed a MAC-derived \
                    bot ID, an ipconfig-style recon report, or a subnet target list. Treat as \
                    evasion only if the read feeds a comparison that gates control flow.",
    },
    ApiSpec {
        key: "GetAdaptersAddresses",
        always_noise: false,
        severity: medium(),
        reasoning: "Reads adapter details including MAC addresses (GetAdaptersAddresses). \
                    Dual-use: VM checks match the MAC against hypervisor OUI prefixes; at least \
                    as often the values seed a MAC-derived bot ID or network recon output. Treat \
                    as evasion only if the read feeds a comparison that gates control flow.",
    },
    ApiSpec {
        key: "SetupDiGetClassDevs",
        always_noise: false,
        severity: medium(),
        reasoning: "Enumerates installed devices/drivers (SetupDiGetClassDevs). Dual-use: VM \
                    checks hunt guest drivers (vboxguest, vmci, vmhgfs), but the same call \
                    enumerates printers or USB devices and appears as junk-API padding with a \
                    discarded result. The device-class GUID argument discriminates; treat as \
                    evasion only if the result feeds a comparison that gates control flow.",
    },
    ApiSpec {
        key: "CreateToolhelp32Snapshot",
        always_noise: false,
        severity: medium(),
        reasoning: "Takes a Toolhelp snapshot (CreateToolhelp32Snapshot) of processes, threads, \
                    modules, or heaps per its flags argument. Dual-use: analysis-process hunts \
                    (vmtoolsd, vboxservice, wireshark, procmon) read the list to bail, but \
                    injection-target selection, victim-ID building, and process-inventory recon \
                    walk the same list at least as often. Treat as evasion only if enumerated \
                    names feed a comparison against analysis-tool names that gates control flow.",
    },
    ApiSpec {
        key: "Process32First",
        always_noise: false,
        severity: medium(),
        reasoning: "Walks a process snapshot (Process32First/Next). Dual-use: an analysis-process \
                    hunt compares each name against VM/analysis tools, while injection-target \
                    selection, victim-ID building, and recon drive the same loop at least as \
                    often. Treat as evasion only if the names feed a comparison against \
                    analysis-tool names that gates control flow.",
    },
    ApiSpec {
        key: "Process32Next",
        always_noise: false,
        severity: medium(),
        reasoning: "Walks a process snapshot (Process32First/Next). Dual-use: an analysis-process \
                    hunt compares each name against VM/analysis tools, while injection-target \
                    selection, victim-ID building, and recon drive the same loop at least as \
                    often. Treat as evasion only if the names feed a comparison against \
                    analysis-tool names that gates control flow.",
    },
    ApiSpec {
        key: "FindWindow",
        always_noise: false,
        severity: medium(),
        reasoning: "Looks up a window by class or title (FindWindow). Dual-use: anti-analysis \
                    checks hunt debugger/monitor windows (OllyDbg, x64dbg, Process Hacker), but \
                    programs just as often look up their own windows (e.g. the console class of \
                    a console they just allocated) to hide or redraw them. The window-class \
                    argument discriminates; treat as evasion only if the result feeds a \
                    comparison that gates control flow.",
    },
    ApiSpec {
        key: "wine_get_unix_file_name",
        always_noise: false,
        severity: high(),
        reasoning: "References wine_get_unix_file_name, an export that exists only under the \
                    Wine compatibility layer; probing for it (typically via GetProcAddress) is a \
                    standard not-on-real-Windows check. The probe is evasion when its result \
                    gates control flow.",
    },
];

/// The coarse system-survey APIs retired from standalone findings.
///
/// Core-count, RAM, and disk-capacity reads are ubiquitous in benign software,
/// and in malware they feed recon reports and C2 beacon fields far more often
/// than sandbox thresholds, so a per-function lead asserts nothing checkable.
/// When any of these is imported *and* called, the detector emits one
/// file-level inventory line instead (see [`survey_observation`]); uncalled
/// imports of them say nothing at all.
const SURVEY_APIS: &[&str] = &[
    "GetSystemInfo",
    "GetNativeSystemInfo",
    "GlobalMemoryStatusEx",
    "GetDiskFreeSpaceEx",
    "GetDiskFreeSpace",
];

/// The Toolhelp walk APIs folded into one process-walk finding per function.
///
/// One enumeration loop otherwise yields two to six near-identical leads (the
/// snapshot plus `Process32First`/`Process32Next`, each split again across
/// thunked and direct call sites), every one disputed or ignored separately.
const TOOLHELP_TRIO: &[&str] = &[
    "CreateToolhelp32Snapshot",
    "Process32First",
    "Process32Next",
];

/// The merged process-walk finding's reasoning: the dual uses process
/// enumeration actually serves, plus the discriminator that would make the
/// evasion reading real.
const WALK_REASONING: &str = "One Toolhelp enumeration: the snapshot/walk call sites in this function are a single \
     behavior, folded into one finding. Dual-use: process-list walking serves \
     injection-target selection (svchost/explorer/winlogon pickers), victim-ID and C2 \
     process-inventory building, and plain recon at least as often as sandbox evasion. The \
     evasion reading — hunting analysis processes like vmtoolsd, vboxservice, wireshark, or \
     procmon — holds only if each enumerated name feeds a comparison against such names that \
     gates control flow, which this pass does not itself verify (the validation pass may \
     later confirm it). Absent that comparison, treat this as process discovery (T1057) in \
     service of the uses above.";

/// VM/hypervisor product, driver, and device artifact strings.
const VM_STRINGS: &[&str] = &[
    "vbox",
    "vboxguest",
    "vboxservice",
    "vboxmouse",
    "vboxsf",
    "vmusbmouse",
    "virtualbox",
    "vmware",
    "vmtoolsd",
    "vmci",
    "vmhgfs",
    "vmmouse",
    "qemu",
    "xenservice",
    "\\\\.\\vboxguest",
    "\\\\.\\hgfs",
    "prl_cc",
    "parallels",
    "sandboxie",
    "sbiedll",
];

/// Matching options for the VM artifact needles.
///
/// The base64-run veto keeps short product names (`"vbox"`, `"qemu"`) from
/// matching inside key material and encoded blobs. Letter-boundary matching is
/// deliberately *not* used here: the genuine artifacts are camel-case
/// compounds whose needles are letter-flanked (`"vbox"` inside
/// `VBoxTrayToolWnd`, `"vmtoolsd"` inside `vmtoolsdControlWndClass`,
/// `"vmware"` inside `VMwareTrayIcon` — all confirmed values), so a letter
/// bound would suppress exactly what this sub-pass gets right.
const VM_MATCH_OPTIONS: MatchOptions = MatchOptions {
    mode: MatchMode::Substring,
    base64_run_veto: true,
    shape: None,
};

/// Matching options for the path-shaped needles (registry keys, MAC OUI
/// prefixes, Linux probe paths).
///
/// These needles are delimiter-bounded fragments (`\`, `:`, `/`, `_` flank
/// every genuine use), so a letter directly against the match means a
/// different token (`"disk\\enum"` sliced out of `Disk\\Enumerate...`) and is
/// safely vetoed; the base64-run veto drops coincidences inside encoded data.
const PATH_MATCH_OPTIONS: MatchOptions = MatchOptions {
    mode: MatchMode::LetterBounded,
    base64_run_veto: true,
    shape: None,
};

/// VM registry-key artifact strings.
const VM_REGISTRY_STRINGS: &[&str] = &[
    "hardware\\acpi\\dsdt\\vbox__",
    "system\\currentcontrolset\\services\\vboxguest",
    "disk\\enum",
    "ven_vmware",
];

/// Hypervisor virtual-NIC MAC-address OUI prefixes.
const MAC_PREFIX_STRINGS: &[&str] = &[
    "08:00:27", "00:05:69", "00:0c:29", "00:1c:14", "00:50:56", "00:16:3e", "00:1c:42",
];

/// The Linux DMI product-name probe path — a VM tell with no static-libc source.
const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";

/// The `/proc/cpuinfo` probe path, matched only beside a hypervisor tell.
///
/// Every statically linked libc embeds this path via `get_nprocs()`, so alone
/// it is library furniture; a genuine hypervisor-flag read co-occurs with one
/// of [`CPUINFO_TELLS`].
const PROC_CPUINFO: &str = "/proc/cpuinfo";

/// Defined-string tells that make a `/proc/cpuinfo` reference a plausible
/// hypervisor-flag read rather than a libc core-count helper.
const CPUINFO_TELLS: &[&str] = &["hypervisor", "/sys/class/dmi"];

#[async_trait::async_trait]
impl Detection for SandboxSystemChecks {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "sandbox_system_checks"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1497_001
    }

    /// Run all System Checks sub-passes and merge their hits.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context
    #[tracing::instrument(name = "SandboxSystemChecks::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from every sub-pass
        let mut hits = Vec::new();
        // 1) CPUID hypervisor probes (decoded instruction + leaf confirmation)
        hits.extend(
            scan_instructions(ctx, |c| {
                // only CPUID instructions are of interest
                if c.mnemonic() != "CPUID" {
                    return None;
                }
                // recover the leaf selector from a preceding `mov eax, <leaf>`
                let leaf = reg_value_before(c.preceding(8), "eax", "rax");
                // vendor leaves (0x40000000..=0x400000FF) are an unambiguous hypervisor query
                let vendor_leaf =
                    matches!(leaf, Some(l) if (0x4000_0000..=0x4000_00FF).contains(&l));
                // leaf 1 only counts if ECX bit 31 (0x80000000) is tested afterwards
                let hyper_bit = matches!(leaf, Some(1)) && tests_hypervisor_bit(c.following(14));
                if !vendor_leaf && !hyper_bit {
                    // ordinary CPUID feature detection — ignore (avoids false positives)
                    return None;
                }
                // classify + record the hypervisor probe
                let label = if vendor_leaf {
                    "CPUID hypervisor vendor query"
                } else {
                    "CPUID hypervisor-present bit check (leaf 1, ECX bit 31)"
                };
                Some(DetectionHit::new(
                    T1497_001,
                    c.addr(),
                    Some(c.func_entry),
                    high(),
                    format!("{label} @ {}", fmt_addr(c.addr())),
                    "Uses CPUID to query for a hypervisor (vendor leaf 0x40000000+, or leaf 1 with \
                     the ECX bit-31 'hypervisor present' test). Malware reads this to tell bare metal \
                     from a VM/sandbox and may halt if virtualized. Defeat by masking the hypervisor \
                     bit or running on bare metal."
                        .to_string(),
                ).kind(EvidenceKind::ControlFlowGated))
            })
            .await?,
        );
        // 2) VM/hypervisor artifact strings (products/drivers/devices), with the
        // bare encryption-target extensions ('.vbox' beside .vhd/.vmdk) vetoed
        let extension_values = extension_veto_values(ctx);
        hits.extend(drop_extension_string_hits(
            string_artifact_hits_with(
                ctx,
                T1497_001,
                VM_STRINGS,
                medium(),
                "Contains a virtualization/hypervisor product, driver, or device-path artifact; \
                 malware matches these to conclude it is in a VM/sandbox and then halts or behaves \
                 benignly. Defeat by hiding the artifact or running on bare metal.",
                &[],
                VM_MATCH_OPTIONS,
            )
            .await?,
            &extension_values,
        ));
        // VM registry keys
        hits.extend(
            string_artifact_hits_with(
                ctx,
                T1497_001,
                VM_REGISTRY_STRINGS,
                medium(),
                "Contains a VM registry-key path used to fingerprint virtualization (VM services, \
                 virtual disk enumeration, or ACPI tables).",
                &[],
                PATH_MATCH_OPTIONS,
            )
            .await?,
        );
        // hypervisor MAC OUI prefixes
        hits.extend(
            string_artifact_hits_with(
                ctx,
                T1497_001,
                MAC_PREFIX_STRINGS,
                medium(),
                "Contains a MAC-address OUI prefix assigned to a hypervisor virtual NIC (VMware, \
                 VirtualBox, Xen, Parallels); matching the host MAC against these detects a VM.",
                &[],
                PATH_MATCH_OPTIONS,
            )
            .await?,
        );
        // Linux VM tells: the DMI product name always, /proc/cpuinfo only when a
        // hypervisor tell makes it more than static-libc furniture
        let mut linux_needles: Vec<&str> = vec![DMI_PRODUCT_NAME];
        if has_cpuinfo_tell(ctx.strings.iter().map(|string| string.value.as_str())) {
            linux_needles.push(PROC_CPUINFO);
        }
        hits.extend(
            string_artifact_hits_with(
                ctx,
                T1497_001,
                &linux_needles,
                low(),
                "References a Linux virtualization tell (/proc/cpuinfo 'hypervisor' flag or the DMI \
                 product name like 'VirtualBox'/'KVM'); read to detect a VM.",
                &[],
                PATH_MATCH_OPTIONS,
            )
            .await?,
        );
        // 3) system-fingerprinting API call sites, posed as dual-use observations,
        // with the Toolhelp trio folded into one process-walk finding per function
        hits.extend(merge_toolhelp_hits(
            api_call_hits(ctx, T1497_001, APIS, EvidenceKind::ApiResultCheck).await?,
        ));
        // 4) retired survey APIs: one file-level inventory line when any is called
        let present = called_survey_apis(ctx).await?;
        hits.extend(survey_observation(&present));
        Ok(hits)
    }
}

/// Look ahead from a CPUID for a test of ECX bit 31 (immediate `0x80000000`),
/// or a `bt`/`shr`/`sar` against bit index 31.
///
/// # Arguments
///
/// * `following` - The instructions immediately after the CPUID
fn tests_hypervisor_bit(following: &[ghidra_cli::models::Instruction]) -> bool {
    // scan the window for the hypervisor-bit test
    following.iter().any(|insn| {
        let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
        let m = insn.mnemonic.to_ascii_uppercase();
        // the direct immediate form, or a bit-31 shift/test
        ops.contains("0x80000000")
            || ((m.starts_with("BT") || m == "SHR" || m == "SAR") && ops.contains("0x1f"))
    })
}

/// Fold the Toolhelp trio's per-function hits into one process-walk finding
/// each; every other hit passes through untouched.
///
/// A single enumeration loop otherwise seeds up to six near-identical leads
/// (three APIs, thunked and direct sites split), each read and disputed
/// separately. A lone trio hit in a function is left as-is: with nothing to
/// merge, its own catalog template is the more precise claim (a lone snapshot
/// may be walking threads or modules, not processes).
///
/// # Arguments
///
/// * `hits` - The API catalog hits to post-process
fn merge_toolhelp_hits(hits: Vec<DetectionHit>) -> Vec<DetectionHit> {
    // the pass-through hits, in their original order
    let mut out = Vec::with_capacity(hits.len());
    // the trio's call-site hits, grouped by containing function
    let mut groups: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
    for hit in hits {
        // only call-site hits stamped with a trio API group; the census line
        // and every other API pass through untouched
        if hit.api.is_some_and(|api| TOOLHELP_TRIO.contains(&api)) {
            groups.entry(hit.function).or_default().push(hit);
        } else {
            out.push(hit);
        }
    }
    // fold each function's trio hits into one finding (singletons stay as-is)
    for (function, group) in groups {
        if group.len() == 1 {
            out.extend(group);
        } else {
            out.push(merged_walk_hit(function, &group));
        }
    }
    out
}

/// Build the one merged process-walk finding for a function's trio hits.
///
/// Carries the union of every member's evidence sites, the strongest member
/// severity (and strongest clamped-aside catalog severity, when any), and the
/// [`EvidenceKind::ApiResultCheck`] contract so the validation pass can still
/// confirm a gating use across all sites.
///
/// # Arguments
///
/// * `function` - The containing function the group was keyed by
/// * `group` - The two or more trio hits being folded together
fn merged_walk_hit(function: Option<u64>, group: &[DetectionHit]) -> DetectionHit {
    // union every member's evidence sites (a hit without a site list is backed
    // by its primary address alone)
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
    // sort + dedupe for stable, clean output
    sites.sort_unstable();
    sites.dedup();
    // the distinct trio APIs present, in catalog order
    let keys: Vec<&'static str> = TOOLHELP_TRIO
        .iter()
        .copied()
        .filter(|key| group.iter().any(|hit| hit.api == Some(*key)))
        .collect();
    // the strongest member severity carries into the merged finding
    let severity = group
        .iter()
        .map(|hit| (hit.confidence, hit.suspicion))
        .max_by_key(|(confidence, suspicion)| (confidence_rank(*confidence), *suspicion))
        .unwrap_or_else(medium);
    // the strongest clamped-aside catalog severity carries for the validator
    let catalog = group
        .iter()
        .filter_map(|hit| hit.catalog_severity)
        .max_by_key(|(confidence, suspicion)| (confidence_rank(*confidence), *suspicion));
    // a walk is only claimed when a walk API is present: a snapshot whose
    // thunked and direct sites merged may be walking threads or modules
    let walking = keys.iter().any(|key| *key != "CreateToolhelp32Snapshot");
    let label = if walking {
        "process-list walk"
    } else {
        "Toolhelp snapshot"
    };
    // render the merged content, naming the APIs present and every site
    let names = walk_api_names(&keys);
    let content = match sites.len() {
        1 => format!("{label} ({names}) @ {}", render_walk_sites(&sites)),
        n => format!(
            "{label} ({names}) — {n} sites @ {}",
            render_walk_sites(&sites)
        ),
    };
    // anchor on the lowest site, like the per-API hits do
    let primary = sites.first().copied().unwrap_or(0);
    // assemble the merged finding; no single catalog key describes a
    // multi-API group, so the API stamp is only kept for single-API folds
    let mut hit = DetectionHit::new(
        T1497_001,
        primary,
        function,
        severity,
        content,
        WALK_REASONING,
    )
    .kind(EvidenceKind::ApiResultCheck)
    .sites(sites);
    if let [key] = keys.as_slice() {
        hit = hit.api(key);
    }
    if let Some(catalog) = catalog {
        hit = hit.catalog_severity(catalog);
    }
    // a fold is only inherent noise when every member was asserted to be
    if group.iter().all(|member| member.always_noise) {
        hit = hit.always_noise();
    }
    hit
}

/// Render the distinct trio APIs present in a merged group, in catalog order,
/// collapsing `Process32First`+`Process32Next` into one token.
///
/// # Arguments
///
/// * `keys` - The distinct trio API keys present, in catalog order
fn walk_api_names(keys: &[&'static str]) -> String {
    // the snapshot leads when present
    let mut parts: Vec<&str> = Vec::new();
    if keys.contains(&"CreateToolhelp32Snapshot") {
        parts.push("CreateToolhelp32Snapshot");
    }
    // collapse the walk pair into the idiomatic First/Next rendering
    match (
        keys.contains(&"Process32First"),
        keys.contains(&"Process32Next"),
    ) {
        (true, true) => parts.push("Process32First/Next"),
        (true, false) => parts.push("Process32First"),
        (false, true) => parts.push("Process32Next"),
        (false, false) => {}
    }
    parts.join(" + ")
}

/// The max call sites named in a merged process-walk finding's content.
const MAX_WALK_SITES: usize = 12;

/// Render a merged finding's sites, naming the first few and counting the rest.
///
/// # Arguments
///
/// * `sites` - The merged evidence sites, in ascending address order
fn render_walk_sites(sites: &[u64]) -> String {
    // name the first few sites
    let listed = sites
        .iter()
        .take(MAX_WALK_SITES)
        .map(|site| fmt_addr(*site))
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: say how many sites were left out
    match sites
        .len()
        .checked_sub(MAX_WALK_SITES)
        .filter(|extra| *extra > 0)
    {
        Some(extra) => format!("{listed} (+{extra} more; {} sites in total)", sites.len()),
        None => listed,
    }
}

/// Resolve which survey APIs are both imported and actually called.
///
/// Returns the matched [`SURVEY_APIS`] keys in catalog order; ANSI/Wide twins
/// normalize to one key and count once.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
async fn called_survey_apis(ctx: &AnalysisContext) -> Result<Vec<&'static str>> {
    // the survey APIs proven both imported and called
    let mut present: Vec<&'static str> = Vec::new();
    // walk the import table once, matching normalized names against the list
    for import in &ctx.imports {
        // strip import decorations and the ANSI/Wide suffix for matching
        let norm = normalize(&import.name);
        let Some(key) = SURVEY_APIS.iter().copied().find(|key| *key == norm) else {
            continue;
        };
        // a key already proven called needs no second xref query
        if present.contains(&key) {
            continue;
        }
        // only a resolved call site makes the API part of the observation; an
        // uncalled survey import says nothing worth a line
        let sites = ctx
            .resolve_call_sites(&import.address)
            .await
            .with_context(|| format!("resolving call sites of import '{}'", import.name))?;
        if !sites.is_empty() {
            present.push(key);
        }
    }
    // list the keys in catalog order whatever order the import table used
    present.sort_by_key(|key| SURVEY_APIS.iter().position(|survey| survey == key));
    Ok(present)
}

/// Build the one file-level inventory observation for called survey APIs.
///
/// Returns `None` when no survey API is imported-and-called. The hit is
/// file-level (address 0, no function), [`low`], and context-only — never a
/// per-function finding — because presence of these ubiquitous reads supports
/// an inventory statement but not a sandbox-check claim.
///
/// # Arguments
///
/// * `present` - The called survey APIs' keys, in catalog order
fn survey_observation(present: &[&'static str]) -> Option<DetectionHit> {
    // nothing called means nothing to observe
    if present.is_empty() {
        return None;
    }
    // one file-level line naming every called survey API
    let content = format!("system survey APIs present: {}", present.join(", "));
    // inventory wording only: the evasion claim needs a compared result, which
    // presence alone can never show
    let reasoning = "Inventory observation: the sample reads coarse machine facts (core count, \
                     RAM size, disk capacity) through the listed survey APIs. These reads are \
                     ubiquitous in benign software, and in malware they build recon output or \
                     victim profiles (CPU/RAM fields in C2 beacons, drive surveys) at least as \
                     often as they gate evasion, so no per-function finding is emitted for \
                     them; a sandbox-check claim requires the read result to feed a comparison \
                     that gates control flow."
        .to_string();
    // context-only by construction: there is no per-site claim to validate
    Some(DetectionHit::new(
        T1497_001,
        0,
        None,
        low(),
        content,
        reasoning,
    ))
}

/// Collect the defined-string values the extension-shape veto applies to: raw
/// values whose lower-cased form is a bare dot-prefixed extension that a VM
/// needle matches under the sub-pass's own options.
///
/// # Arguments
///
/// * `ctx` - The analysis context
fn extension_veto_values(ctx: &AnalysisContext) -> Vec<&str> {
    // examine every defined string the way the sub-pass will
    ctx.strings
        .iter()
        .filter_map(|string| {
            // shape-test the lower-cased value the matcher sees
            let lower = string.value.to_ascii_lowercase();
            let vetoed = extension_shaped(&lower)
                && VM_STRINGS
                    .iter()
                    .any(|needle| needle_matches(&lower, needle, VM_MATCH_OPTIONS));
            // keep the raw value: the hit content renders the original casing
            vetoed.then_some(string.value.as_str())
        })
        .collect()
}

/// Whether a whole (lower-cased) string value is a bare dot-prefixed file
/// extension (`.vbox`, `.vmdk` style: a dot then one to six alphanumerics).
///
/// A VM product name in that shape is an encryption-target or filetype list
/// entry, not a virtualization artifact.
///
/// # Arguments
///
/// * `value` - The lower-cased defined-string value
fn extension_shaped(value: &str) -> bool {
    // a bare extension starts with its dot
    let Some(rest) = value.strip_prefix('.') else {
        return false;
    };
    // one to six lower-case alphanumerics and nothing else is the list shape
    (1..=6).contains(&rest.len())
        && rest
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// Drop the VM-string hits whose matched string is one of the vetoed bare
/// extensions, keeping every other hit untouched.
///
/// The sub-pass renders a string hit's content as `string "<value>"`,
/// optionally followed by the reference list, so a vetoed value identifies its
/// hits exactly (extension-shaped values are short enough that the rendering
/// never truncates or escapes them).
///
/// # Arguments
///
/// * `hits` - The VM-string sub-pass hits
/// * `vetoed` - The raw extension-shaped values to drop
fn drop_extension_string_hits(hits: Vec<DetectionHit>, vetoed: &[&str]) -> Vec<DetectionHit> {
    // nothing to veto: hand the hits back untouched
    if vetoed.is_empty() {
        return hits;
    }
    // keep every hit that names none of the vetoed values
    hits.into_iter()
        .filter(|hit| {
            !vetoed.iter().any(|value| {
                // reconstruct the exact content the sub-pass renders for the value
                let exact = format!("string {value:?}");
                hit.content == exact
                    || hit
                        .content
                        .strip_prefix(&exact)
                        .is_some_and(|rest| rest.starts_with(" referenced @ "))
            })
        })
        .collect()
}

/// Whether any defined string carries a tell that makes a `/proc/cpuinfo`
/// reference a plausible hypervisor-flag read (see [`CPUINFO_TELLS`]).
///
/// # Arguments
///
/// * `values` - The defined-string values to scan
fn has_cpuinfo_tell<'a>(values: impl IntoIterator<Item = &'a str>) -> bool {
    // one tell anywhere in the string table is enough
    values.into_iter().any(|value| {
        // compare case-insensitively, like the string matcher does
        let lower = value.to_ascii_lowercase();
        CPUINFO_TELLS.iter().any(|tell| lower.contains(tell))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::confidence::Confidence;

    /// Build an API call-site hit shaped like the catalog pass's output.
    ///
    /// # Arguments
    ///
    /// * `api` - The normalized catalog API key
    /// * `function` - The containing function entry, when known
    /// * `site` - The call-site address
    fn api_hit(api: &'static str, function: Option<u64>, site: u64) -> DetectionHit {
        DetectionHit::new(
            T1497_001,
            site,
            function,
            medium(),
            format!("{api} call @ {}", fmt_addr(site)),
            format!("{api} catalog reasoning"),
        )
        .kind(EvidenceKind::ApiResultCheck)
        .api(api)
    }

    /// The survey APIs are retired: none has a per-function catalog entry.
    #[test]
    fn survey_apis_are_retired_from_the_catalog() {
        // a disputed GetSystemInfo lead (dwNumberOfProcessors fed
        // straight into a wsprintfW C2 report, never compared or branched on)
        // can no longer seed a per-function finding, nor can the disk surveys
        for key in SURVEY_APIS {
            assert!(
                APIS.iter().all(|spec| spec.key != *key),
                "{key} must be retired from the per-function catalog"
            );
        }
    }

    /// Called survey APIs fold into one low, context-only file-level line.
    #[test]
    fn survey_observation_is_one_file_level_inventory_line() {
        // two called survey APIs make one combined observation
        let hit = survey_observation(&["GetSystemInfo", "GlobalMemoryStatusEx"])
            .expect("an inventory observation");
        // file-level, weakest grade, context-only: never a per-function finding
        assert_eq!(hit.address, 0);
        assert_eq!(hit.function, None);
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, low().1);
        assert!(matches!(hit.kind, EvidenceKind::ContextOnly));
        assert_eq!(
            hit.content,
            "system survey APIs present: GetSystemInfo, GlobalMemoryStatusEx"
        );
        // the wording stays an inventory statement with the discriminator named
        assert!(hit.reasoning.contains("Inventory observation"));
        assert!(hit.reasoning.contains("comparison that gates control flow"));
        // and nothing is observed when no survey API is called
        assert!(survey_observation(&[]).is_none());
    }

    /// The Toolhelp trio in one function merges into one process-walk finding.
    #[test]
    fn toolhelp_trio_merges_into_one_walk_per_function() {
        // fn-18000d750's disputed shape: three separate leads (snapshot @
        // 0x18000d78a, Process32First @ 0x18000d7a1, Process32Next @
        // 0x18000d7cd) all answered by the same winlogon.exe token-theft
        // disproof, folded into one finding
        let hits = vec![
            api_hit(
                "CreateToolhelp32Snapshot",
                Some(0x1_8000_d750),
                0x1_8000_d78a,
            ),
            api_hit("Process32First", Some(0x1_8000_d750), 0x1_8000_d7a1),
            api_hit("Process32Next", Some(0x1_8000_d750), 0x1_8000_d7cd),
        ];
        let merged = merge_toolhelp_hits(hits);
        assert_eq!(merged.len(), 1);
        let walk = &merged[0];
        // one finding carrying every site, anchored at the lowest
        assert_eq!(walk.function, Some(0x1_8000_d750));
        assert_eq!(
            walk.sites,
            vec![0x1_8000_d78a, 0x1_8000_d7a1, 0x1_8000_d7cd]
        );
        assert_eq!(walk.address, 0x1_8000_d78a);
        // the strongest member severity carries, with the validator contract
        assert!(matches!(walk.confidence, Confidence::Unsure));
        assert_eq!(walk.suspicion, medium().1);
        assert!(matches!(walk.kind, EvidenceKind::ApiResultCheck));
        // the content names the whole trio and the site count
        assert!(
            walk.content
                .starts_with("process-list walk (CreateToolhelp32Snapshot + Process32First/Next)")
        );
        assert!(walk.content.contains("3 sites"));
        // no single catalog API describes a multi-API group
        assert_eq!(walk.api, None);
        // the reasoning poses the dual uses instead of asserting the VM hunt
        assert!(walk.reasoning.contains("injection-target selection"));
        assert!(walk.reasoning.contains("victim-ID"));
        assert!(walk.reasoning.contains("recon"));
        assert!(walk.reasoning.contains("comparison against such names"));
    }

    /// Walks merge per function; other APIs' hits pass through untouched.
    #[test]
    fn trio_merge_groups_by_function_and_leaves_others_alone() {
        // fn-180004e10's multi-call pattern and a lone Process32Next in another
        // function, plus an own-console FindWindow hit
        let find_window = api_hit("FindWindow", Some(0x40_1000), 0x40_2635);
        let hits = vec![
            api_hit(
                "CreateToolhelp32Snapshot",
                Some(0x1_8000_4e10),
                0x1_8000_4e20,
            ),
            api_hit("Process32First", Some(0x1_8000_4e10), 0x1_8000_4e40),
            api_hit("Process32Next", Some(0x1_8000_d750), 0x1_8000_d7cd),
            find_window.clone(),
        ];
        let merged = merge_toolhelp_hits(hits);
        assert_eq!(merged.len(), 3);
        // the non-trio hit passes through byte-identical
        assert_eq!(merged[0].content, find_window.content);
        assert_eq!(merged[0].api, Some("FindWindow"));
        // the two-API function folds into one walk naming only what it saw
        assert!(
            merged[1]
                .content
                .starts_with("process-list walk (CreateToolhelp32Snapshot + Process32First)")
        );
        assert_eq!(merged[1].function, Some(0x1_8000_4e10));
        // the lone Process32Next elsewhere stays a separate per-API hit
        assert_eq!(merged[2].function, Some(0x1_8000_d750));
        assert_eq!(merged[2].api, Some("Process32Next"));
    }

    /// A lone snapshot hit is not relabeled as a process walk.
    #[test]
    fn lone_snapshot_hit_passes_through_unchanged() {
        // fn-68d61be0's snapshot leads were Thread32 quiesce loops
        // (TH32CS_SNAPTHREAD, no name compares), so a lone snapshot must keep
        // its own hypothesis template rather than gain a process-walk label
        let hit = api_hit("CreateToolhelp32Snapshot", Some(0x68d6_1be0), 0x68d6_1c00);
        let merged = merge_toolhelp_hits(vec![hit.clone()]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].content, hit.content);
        assert_eq!(merged[0].reasoning, hit.reasoning);
        assert_eq!(merged[0].api, Some("CreateToolhelp32Snapshot"));
    }

    /// Thunked and direct sites of one walk API fold into one lead keeping its key.
    #[test]
    fn same_api_thunk_and_direct_sites_fold_together() {
        // the per-loop multi-emission shape: one API, two split hits, one function
        let hits = vec![
            api_hit("Process32Next", Some(0x40_1000), 0x40_1010),
            api_hit("Process32Next", Some(0x40_1000), 0x40_1050),
        ];
        let merged = merge_toolhelp_hits(hits);
        assert_eq!(merged.len(), 1);
        // a single-API fold keeps its normalized key for the validator
        assert_eq!(merged[0].api, Some("Process32Next"));
        assert_eq!(merged[0].sites, vec![0x40_1010, 0x40_1050]);
        assert!(
            merged[0]
                .content
                .starts_with("process-list walk (Process32Next)")
        );
    }

    /// A snapshot-only fold is labeled a snapshot, not a process walk.
    #[test]
    fn snapshot_only_fold_does_not_claim_a_process_walk() {
        // two snapshot sites with no walk API in the function: the enumeration
        // target (processes/threads/modules) is unknown, so no walk is claimed
        let hits = vec![
            api_hit("CreateToolhelp32Snapshot", Some(0x40_2000), 0x40_2010),
            api_hit("CreateToolhelp32Snapshot", Some(0x40_2000), 0x40_2080),
        ];
        let merged = merge_toolhelp_hits(hits);
        assert_eq!(merged.len(), 1);
        assert!(
            merged[0]
                .content
                .starts_with("Toolhelp snapshot (CreateToolhelp32Snapshot)")
        );
        assert_eq!(merged[0].api, Some("CreateToolhelp32Snapshot"));
    }

    /// A confirmed file-level First+Next pair merges and stays medium.
    #[test]
    fn confirmed_file_level_pair_merges_still_confirmable() {
        // both hits are file-level (no containing function), like the
        // confirmed pair: they merge into one still-confirmable walk lead
        let hits = vec![
            api_hit("Process32First", None, 0x40_2000),
            api_hit("Process32Next", None, 0x40_2040),
        ];
        let merged = merge_toolhelp_hits(hits);
        assert_eq!(merged.len(), 1);
        // the confirmed pattern keeps seeding at its current strength
        assert!(matches!(merged[0].confidence, Confidence::Unsure));
        assert_eq!(merged[0].suspicion, medium().1);
        assert!(
            merged[0]
                .content
                .starts_with("process-list walk (Process32First/Next)")
        );
        assert_eq!(merged[0].function, None);
    }

    /// Every catalog template poses a hypothesis with the gating discriminator.
    #[test]
    fn catalog_reasonings_pose_hypotheses_with_a_discriminator() {
        // the asserted-intent templates drove most of the disputed bodies:
        // one sample's GetComputerNameA fed a ROR13-hashed victim ID,
        // another's GetUserNameW a CRC32-hashed bot ID, and the adapter
        // reads WannaCry-style subnet target lists — so every template must
        // name the gating comparison instead of asserting the evasion story
        for spec in APIS {
            assert!(
                spec.reasoning.contains("gates control flow"),
                "{} must name the gating discriminator",
                spec.key
            );
        }
        // the dual-use templates pose both readings for the fingerprint reads
        for key in [
            "GetComputerName",
            "GetUserName",
            "GetAdaptersInfo",
            "GetAdaptersAddresses",
        ] {
            let spec = APIS
                .iter()
                .find(|spec| spec.key == key)
                .expect("the fingerprint APIs stay cataloged");
            // the observation keeps seeding at its current strength
            assert_eq!(spec.severity.1, medium().1);
            assert!(
                spec.reasoning.contains("Dual-use"),
                "{key} must pose the dual-use question"
            );
        }
        // the builder shapes the disputes proved are named, not denied
        let computer = APIS
            .iter()
            .find(|spec| spec.key == "GetComputerName")
            .expect("GetComputerName stays cataloged");
        assert!(computer.reasoning.contains("victim ID"));
        let adapters = APIS
            .iter()
            .find(|spec| spec.key == "GetAdaptersInfo")
            .expect("GetAdaptersInfo stays cataloged");
        assert!(adapters.reasoning.contains("subnet target list"));
        // the old asserted-intent phrasings are gone
        assert!(
            APIS.iter()
                .all(|spec| !spec.reasoning.contains("and evade if matched"))
        );
        assert!(
            APIS.iter()
                .all(|spec| !spec.reasoning.contains("and bail if present"))
        );
    }

    /// Bare dot-prefixed extensions are vetoed; real VM artifacts are kept.
    #[test]
    fn extension_shaped_vm_strings_are_vetoed() {
        // a '.vbox' string sat in an encryption-target extension list
        // beside .vhd/.vmdk/.vdi — an impact tell, not a sandbox check
        assert!(extension_shaped(".vbox"));
        assert!(extension_shaped(".vmdk"));
        // product names, paths, and composite names are not the list shape
        assert!(!extension_shaped("vbox"));
        assert!(!extension_shaped(".virtualbox7"));
        assert!(!extension_shaped(".vbox.bak"));
        assert!(!extension_shaped("c:\\vms\\win10.vbox"));
        assert!(!extension_shaped(""));
        // the veto drops the bare-extension hits, referenced or not, and
        // keeps the confirmed window-class artifact beside them
        let referenced = DetectionHit::new(
            T1497_001,
            0x40_5ac8,
            None,
            medium(),
            "string \".vbox\" referenced @ 0x405ac8".to_string(),
            "r".to_string(),
        );
        let unreferenced = DetectionHit::new(
            T1497_001,
            0x40_5b00,
            None,
            low(),
            "string \".vbox\"".to_string(),
            "r".to_string(),
        );
        let confirmed = DetectionHit::new(
            T1497_001,
            0x40_1000,
            None,
            medium(),
            "string \"VBoxTrayToolWnd\" referenced @ 0x401000".to_string(),
            "r".to_string(),
        );
        let kept =
            drop_extension_string_hits(vec![referenced, unreferenced, confirmed], &[".vbox"]);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].content.contains("VBoxTrayToolWnd"));
    }

    /// Every confirmed VM artifact string keeps matching under the pass options.
    #[test]
    fn confirmed_vm_artifacts_survive_the_match_options() {
        // the full confirmed-string set from the telemetry: camel-case
        // compounds letter-flank their needles ("vbox" in vboxtraytoolwnd),
        // which is exactly why the VM needles stay bare-substring matched
        let confirmed = [
            "vbox",
            "vmware",
            "qemu",
            "vmtoolsdcontrolwndclass",
            "software\\vmware, inc.",
            "vmwaredragdetwndclass",
            "vboxtraytoolwnd",
            "vboxtraytoolwndclass",
            "vmwaretrayicon",
            "sbiedll.dll",
            "vboxservice.exe",
        ];
        for value in confirmed {
            assert!(
                VM_STRINGS
                    .iter()
                    .any(|needle| needle_matches(value, needle, VM_MATCH_OPTIONS)),
                "{value} must keep matching the VM needles"
            );
            // none of the confirmed values is a bare extension
            assert!(!extension_shaped(value), "{value} must not be vetoed");
        }
        // the confirmed registry artifact survives the bounded path options
        assert!(VM_REGISTRY_STRINGS.iter().any(|needle| needle_matches(
            "hardware\\acpi\\dsdt\\vbox__",
            needle,
            PATH_MATCH_OPTIONS
        )));
        // while a product name sliced out of encoded data no longer matches
        let blob = "abcdefghijklmqemunopqrstuvwx";
        assert!(!needle_matches(blob, "qemu", VM_MATCH_OPTIONS));
        assert!(needle_matches(blob, "qemu", MatchOptions::default()));
    }

    /// `/proc/cpuinfo` seeds only beside a hypervisor tell.
    #[test]
    fn cpuinfo_requires_a_hypervisor_tell() {
        // uClibc's get_nprocs() gives every statically linked
        // Linux binary a /proc/cpuinfo string, so alone it is library furniture
        assert!(!has_cpuinfo_tell(["/proc/cpuinfo", "get_nprocs"]));
        // a hypervisor-flag read or the DMI probe pairs it back in
        assert!(has_cpuinfo_tell(["/proc/cpuinfo", "hypervisor"]));
        assert!(has_cpuinfo_tell(["/sys/class/dmi/id/product_name"]));
        // the tell scan folds case like the string matcher does
        assert!(has_cpuinfo_tell(["HYPERVISOR"]));
    }
}
