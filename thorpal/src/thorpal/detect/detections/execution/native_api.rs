//! Native API / direct-syscall abuse (MITRE ATT&CK **T1106**).
//!
//! Detects malware that reaches the kernel through the *lowest-level* interface
//! available so its behavior does not surface at the monitored Win32 layer:
//! `ntdll` `Nt*`/`Zw*` calls (native twins of Win32 file/process/memory/registry/
//! loader APIs — calling `NtWriteFile` instead of `WriteFile` dodges hooks on
//! kernel32/kernelbase), and direct `SYSCALL`/`INT 0x2E` (skipping `ntdll` so even
//! native-API hooks are evaded). Direct syscalls are flagged only in PE targets:
//! in a static ELF every libc call is a syscall, so the raw opcode carries no
//! signal there (the notable Linux case, `ptrace`, is handled by debugger_evasion).
//!
//! APIs catalogued by other detections (`NtClose`, `NtQueryInformationProcess`,
//! `NtDelayExecution`) are intentionally not duplicated here.

use anyhow::Result;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, high, medium,
    scan_instructions,
};

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
        severity: medium(),
        reasoning: "Calls native NtCreateFile instead of Win32 CreateFile; the ntdll form bypasses \
                    user-mode hooks on kernel32/kernelbase, so file activity is not seen at the \
                    monitored Win32 layer.",
    },
    ApiSpec {
        key: "NtOpenFile",
        severity: medium(),
        reasoning: "Calls native NtOpenFile instead of CreateFile/OpenFile to open a handle below \
                    the monitored Win32 layer.",
    },
    ApiSpec {
        key: "NtReadFile",
        severity: medium(),
        reasoning: "Calls native NtReadFile instead of Win32 ReadFile; reading files via ntdll evades \
                    hooks on the Win32 file APIs, a common ransomware and stealer pattern.",
    },
    ApiSpec {
        key: "NtWriteFile",
        severity: medium(),
        reasoning: "Calls native NtWriteFile instead of Win32 WriteFile; writing files via ntdll \
                    evades hooks on the Win32 file APIs, a common ransomware and wiper pattern.",
    },
    ApiSpec {
        key: "NtQueryDirectoryFile",
        severity: medium(),
        reasoning: "Calls native NtQueryDirectoryFile instead of FindFirstFile/FindNextFile to \
                    enumerate a directory without the monitored Win32 enumeration APIs.",
    },
    ApiSpec {
        key: "NtSetInformationFile",
        severity: medium(),
        reasoning: "Calls native NtSetInformationFile (rename/delete-on-close) instead of the Win32 \
                    file APIs, a common native-API way to rename or delete files unseen.",
    },
    ApiSpec {
        key: "NtOpenProcess",
        severity: medium(),
        reasoning: "Calls native NtOpenProcess instead of OpenProcess to obtain a process handle \
                    below the Win32 layer, typically a precursor to native-API injection.",
    },
    ApiSpec {
        key: "NtAllocateVirtualMemory",
        severity: medium(),
        reasoning: "Calls native NtAllocateVirtualMemory instead of VirtualAlloc(Ex); allocating \
                    (often RWX) memory via ntdll evades Win32 allocation hooks — a classic injection \
                    building block.",
    },
    ApiSpec {
        key: "NtProtectVirtualMemory",
        severity: high(),
        reasoning: "Calls native NtProtectVirtualMemory instead of VirtualProtect(Ex) to change page \
                    protections (e.g. make a buffer executable) below the Win32 hook layer — a strong \
                    injection/self-modifying-code tell.",
    },
    ApiSpec {
        key: "NtWriteVirtualMemory",
        severity: high(),
        reasoning: "Calls native NtWriteVirtualMemory instead of WriteProcessMemory to write into \
                    another process via ntdll — the core primitive of native-API process injection.",
    },
    ApiSpec {
        key: "NtMapViewOfSection",
        severity: high(),
        reasoning: "Calls native NtMapViewOfSection to map a section into a (remote) process — the \
                    section-mapping injection technique that avoids WriteProcessMemory entirely.",
    },
    ApiSpec {
        key: "NtCreateThreadEx",
        severity: high(),
        reasoning: "Calls native NtCreateThreadEx instead of CreateRemoteThread to start execution in \
                    a (remote) process via ntdll — a hallmark of native-API code injection.",
    },
    ApiSpec {
        key: "NtQueueApcThread",
        severity: high(),
        reasoning: "Calls native NtQueueApcThread to queue an APC into a thread — the APC-injection \
                    execution primitive, performed below the Win32 layer.",
    },
    ApiSpec {
        key: "NtCreateSection",
        severity: medium(),
        reasoning: "Calls native NtCreateSection to create a memory section, commonly paired with \
                    NtMapViewOfSection for section-mapping process injection.",
    },
    ApiSpec {
        key: "NtResumeThread",
        severity: medium(),
        reasoning: "Calls native NtResumeThread to resume a (suspended, possibly hollowed) thread via \
                    ntdll — the trigger step of process hollowing / injection.",
    },
    ApiSpec {
        key: "NtCreateKey",
        severity: medium(),
        reasoning: "Calls native NtCreateKey instead of RegCreateKey(Ex) to create a registry key \
                    below the monitored Win32 registry APIs (e.g. for stealthy persistence).",
    },
    ApiSpec {
        key: "NtSetValueKey",
        severity: medium(),
        reasoning: "Calls native NtSetValueKey instead of RegSetValue(Ex) to write a registry value \
                    without the monitored Win32 registry APIs (e.g. a Run-key persistence value).",
    },
    ApiSpec {
        key: "NtQueryValueKey",
        severity: medium(),
        reasoning: "Calls native NtQueryValueKey instead of RegQueryValue(Ex) to read the registry \
                    below the Win32 layer.",
    },
    ApiSpec {
        key: "LdrLoadDll",
        severity: medium(),
        reasoning: "Calls ntdll LdrLoadDll directly instead of LoadLibrary; manual module loading via \
                    the loader API bypasses hooks on LoadLibrary and hides which DLLs are pulled in.",
    },
    ApiSpec {
        key: "LdrGetProcedureAddress",
        severity: medium(),
        reasoning: "Calls ntdll LdrGetProcedureAddress instead of GetProcAddress to resolve API \
                    addresses below the Win32 layer — dynamic API resolution that hides the IAT.",
    },
];

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

    /// Flag native-API call sites and (PE only) direct system calls.
    #[tracing::instrument(name = "NativeApi::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();

        // 1) native ntdll Nt*/Zw* API call sites (their result gates behavior)
        hits.extend(api_call_hits(ctx, T1106, APIS, EvidenceKind::ApiResultCheck).await?);

        // 2) direct SYSCALL / INT 0x2E — only meaningful on PE targets (see docs)
        if is_pe(ctx) {
            hits.extend(
                scan_instructions(ctx, |c| {
                    let m = c.mnemonic();
                    let ops = c.operands().replace(' ', "");
                    // a hand-rolled system call, skipping ntdll's stubs entirely
                    if m == "SYSCALL" {
                        return Some(
                            DetectionHit::new(
                                T1106,
                                c.addr(),
                                Some(c.func_entry),
                                medium(),
                                format!("direct SYSCALL @ {}", fmt_addr(c.addr())),
                                "Issues a SYSCALL directly instead of calling ntdll. This 'direct \
                             syscall' technique skips the ntdll stubs so even native-API hooks are \
                             bypassed, entering the kernel with no monitored user-mode frame.",
                            )
                            .kind(EvidenceKind::ControlFlowGated),
                        );
                    }
                    // INT 0x2E — the legacy system-call gate, same intent as SYSCALL
                    if m == "INT" && (ops == "0x2e" || ops.ends_with(",0x2e")) {
                        return Some(DetectionHit::new(
                            T1106,
                            c.addr(),
                            Some(c.func_entry),
                            medium(),
                            format!("INT 0x2E system-call gate @ {}", fmt_addr(c.addr())),
                            "Enters the kernel via the legacy INT 0x2E system-call gate instead of \
                             calling ntdll, bypassing user-mode API/native hooks.",
                        )
                        .kind(EvidenceKind::ControlFlowGated));
                    }
                    None
                })
                .await?,
            );
        }

        Ok(hits)
    }
}
