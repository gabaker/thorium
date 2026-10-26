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
//!    VM registry keys, hypervisor MAC-address OUI prefixes, `/proc/cpuinfo`, and
//!    the DMI product name.
//! 3. **System-fingerprint APIs** — reading core count / RAM / disk size /
//!    hostname / username / MAC / installed drivers / running processes and
//!    comparing them against known sandbox values.

use anyhow::Result;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, high, low,
    medium, reg_value_before, scan_instructions, string_artifact_hits,
};

use super::T1497_001;

/// The System Checks detection (ATT&CK T1497.001).
pub struct SandboxSystemChecks;

/// System-fingerprinting APIs used to tell a VM/sandbox from a real host.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetSystemInfo",
        severity: low(),
        reasoning: "Reads the processor count (GetSystemInfo/GetNativeSystemInfo); sandboxes often \
                    expose only 1-2 cores, so a low count is treated as 'probably a sandbox'.",
    },
    ApiSpec {
        key: "GetNativeSystemInfo",
        severity: low(),
        reasoning: "Reads the processor count via GetNativeSystemInfo to spot the low core counts \
                    typical of analysis VMs.",
    },
    ApiSpec {
        key: "GlobalMemoryStatusEx",
        severity: low(),
        reasoning: "Reads installed RAM (GlobalMemoryStatusEx); analysis VMs frequently have small \
                    RAM, so a low value is used as a sandbox indicator.",
    },
    ApiSpec {
        key: "GetDiskFreeSpaceEx",
        severity: low(),
        reasoning: "Reads disk capacity (GetDiskFreeSpaceEx); small virtual disks are typical of \
                    sandboxes, so a small disk is treated as a sign of analysis.",
    },
    ApiSpec {
        key: "GetComputerName",
        severity: medium(),
        reasoning: "Reads the machine name (GetComputerName) to compare against known sandbox \
                    hostnames (e.g. SANDBOX, MALTEST) and evade if matched.",
    },
    ApiSpec {
        key: "GetUserName",
        severity: medium(),
        reasoning: "Reads the current user (GetUserName) to compare against known sandbox usernames \
                    (e.g. WDAGUtilityAccount, HAPUBWS) and evade if matched.",
    },
    ApiSpec {
        key: "GetAdaptersInfo",
        severity: medium(),
        reasoning: "Reads NIC MAC addresses (GetAdaptersInfo); hypervisors use well-known OUI \
                    prefixes (VMware 00:0C:29, VirtualBox 08:00:27, ...) so a match reveals a VM.",
    },
    ApiSpec {
        key: "GetAdaptersAddresses",
        severity: medium(),
        reasoning: "Reads NIC MAC addresses (GetAdaptersAddresses) to match hypervisor OUI prefixes \
                    and detect a virtual machine.",
    },
    ApiSpec {
        key: "SetupDiGetClassDevs",
        severity: medium(),
        reasoning: "Enumerates installed devices/drivers (SetupDiGetClassDevs) to look for VM guest \
                    drivers (vboxguest, vmci, vmhgfs) — a strong virtualization tell.",
    },
    ApiSpec {
        key: "CreateToolhelp32Snapshot",
        severity: medium(),
        reasoning: "Snapshots the process list (CreateToolhelp32Snapshot) to search for analysis \
                    processes (vmtoolsd, vboxservice, wireshark, procmon) and bail if present.",
    },
    ApiSpec {
        key: "Process32First",
        severity: medium(),
        reasoning: "Walks the process list (Process32First/Next), typically hunting for VM/analysis \
                    processes to evade.",
    },
    ApiSpec {
        key: "Process32Next",
        severity: medium(),
        reasoning: "Walks the process list (Process32Next), typically hunting for VM/analysis \
                    processes to evade.",
    },
    ApiSpec {
        key: "FindWindow",
        severity: medium(),
        reasoning: "Calls FindWindow to look for debugger/monitor windows (OllyDbg, x64dbg, Process \
                    Hacker); a match means an analyst tool is open, so the malware evades.",
    },
    ApiSpec {
        key: "wine_get_unix_file_name",
        severity: high(),
        reasoning: "References wine_get_unix_file_name, present only under the Wine compatibility \
                    layer — a definitive sign the sample is not on a real Windows host.",
    },
];

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

/// Linux virtualization-detection artifact strings.
const LINUX_VM_STRINGS: &[&str] = &["/proc/cpuinfo", "/sys/class/dmi/id/product_name"];

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

        // 2) VM/hypervisor artifact strings (products/drivers/devices)
        hits.extend(
            string_artifact_hits(
                ctx,
                T1497_001,
                VM_STRINGS,
                medium(),
                "Contains a virtualization/hypervisor product, driver, or device-path artifact; \
                 malware matches these to conclude it is in a VM/sandbox and then halts or behaves \
                 benignly. Defeat by hiding the artifact or running on bare metal.",
                &[],
            )
            .await?,
        );
        // VM registry keys
        hits.extend(
            string_artifact_hits(
                ctx,
                T1497_001,
                VM_REGISTRY_STRINGS,
                medium(),
                "Contains a VM registry-key path used to fingerprint virtualization (VM services, \
                 virtual disk enumeration, or ACPI tables).",
                &[],
            )
            .await?,
        );
        // hypervisor MAC OUI prefixes
        hits.extend(
            string_artifact_hits(
                ctx,
                T1497_001,
                MAC_PREFIX_STRINGS,
                medium(),
                "Contains a MAC-address OUI prefix assigned to a hypervisor virtual NIC (VMware, \
                 VirtualBox, Xen, Parallels); matching the host MAC against these detects a VM.",
                &[],
            )
            .await?,
        );
        // Linux VM tells (/proc/cpuinfo, DMI product name)
        hits.extend(
            string_artifact_hits(
                ctx,
                T1497_001,
                LINUX_VM_STRINGS,
                low(),
                "References a Linux virtualization tell (/proc/cpuinfo 'hypervisor' flag or the DMI \
                 product name like 'VirtualBox'/'KVM'); read to detect a VM.",
                &[],
            )
            .await?,
        );

        // 3) system-fingerprinting API call sites (results compared to sandbox values)
        hits.extend(api_call_hits(ctx, T1497_001, APIS, EvidenceKind::ApiResultCheck).await?);

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
