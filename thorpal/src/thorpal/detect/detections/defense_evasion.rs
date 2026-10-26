//! MITRE ATT&CK tactic: **Defense Evasion** (TA0005).
//!
//! This is where all of the tool's current detections live, because
//! anti-analysis behavior (anti-debugging, anti-VM/sandbox, anti-instrumentation,
//! timing/sleep evasion, user-activity checks) is classified by ATT&CK under
//! Defense Evasion — specifically the techniques:
//!
//! * **T1622** — Debugger Evasion
//! * **T1497.001** — Virtualization/Sandbox Evasion: System Checks
//! * **T1497.002** — Virtualization/Sandbox Evasion: User Activity Based Checks
//! * **T1497.003** — Virtualization/Sandbox Evasion: Time Based Evasion
//! * **T1027.002** — Obfuscated Files or Information: Software Packing
//! * **T1480.001** — Execution Guardrails: Environmental Keying
//!
//! Additional ATT&CK tactics (Discovery, Execution, Persistence, Command and
//! Control, …) are added as sibling modules under `detections/`. A detection
//! here may still report a secondary technique from another tactic when the same
//! evidence supports both — the host-binding detection reports the identity read
//! itself under Discovery (T1082) while its primary claim stays Defense Evasion.

pub mod anti_instrumentation;
pub mod debugger_evasion;
pub mod embedded_content;
pub mod encoded_strings;
pub mod host_binding;
pub mod packing;
pub mod sandbox_system_checks;
pub mod sandbox_time_evasion;
pub mod sandbox_user_activity;
pub mod self_deletion;
pub mod signature_anomaly;

use std::borrow::Cow;

use crate::thorpal::detect::detections::{AttackTactic, AttackTechnique, Detection};

/// ATT&CK T1622 — Debugger Evasion.
pub const T1622: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1622"),
    name: Cow::Borrowed("Debugger Evasion"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1497.001 — Virtualization/Sandbox Evasion: System Checks.
pub const T1497_001: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1497.001"),
    name: Cow::Borrowed("Virtualization/Sandbox Evasion: System Checks"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1497.002 — Virtualization/Sandbox Evasion: User Activity Based Checks.
pub const T1497_002: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1497.002"),
    name: Cow::Borrowed("Virtualization/Sandbox Evasion: User Activity Based Checks"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1497.003 — Virtualization/Sandbox Evasion: Time Based Evasion.
pub const T1497_003: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1497.003"),
    name: Cow::Borrowed("Virtualization/Sandbox Evasion: Time Based Evasion"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1070.004 — Indicator Removal: File Deletion.
pub const T1070_004: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1070.004"),
    name: Cow::Borrowed("Indicator Removal: File Deletion"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1027 — Obfuscated Files or Information.
pub const T1027: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1027"),
    name: Cow::Borrowed("Obfuscated Files or Information"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1027.002 — Obfuscated Files or Information: Software Packing.
pub const T1027_002: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1027.002"),
    name: Cow::Borrowed("Obfuscated Files or Information: Software Packing"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1480.001 — Execution Guardrails: Environmental Keying.
pub const T1480_001: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1480.001"),
    name: Cow::Borrowed("Execution Guardrails: Environmental Keying"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1082 — System Information Discovery.
///
/// Reported by the host-binding detection for the identity read itself, which is
/// Discovery rather than Defense Evasion — the guardrail claim is the separate
/// [`T1480_001`] finding.
pub const T1082: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1082"),
    name: Cow::Borrowed("System Information Discovery"),
    tactic: AttackTactic::Discovery,
};

/// ATT&CK T1140 — Deobfuscate/Decode Files or Information.
pub const T1140: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1140"),
    name: Cow::Borrowed("Deobfuscate/Decode Files or Information"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1027.009 — Obfuscated Files or Information: Embedded Payloads.
pub const T1027_009: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1027.009"),
    name: Cow::Borrowed("Obfuscated Files or Information: Embedded Payloads"),
    tactic: AttackTactic::DefenseEvasion,
};

/// ATT&CK T1553.002 — Subvert Trust Controls: Code Signing.
pub const T1553_002: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1553.002"),
    name: Cow::Borrowed("Subvert Trust Controls: Code Signing"),
    tactic: AttackTactic::DefenseEvasion,
};

/// The Defense Evasion detections, instantiated for the registry.
pub fn detections() -> Vec<Box<dyn Detection>> {
    // one detection struct per technique-family module
    vec![
        Box::new(debugger_evasion::DebuggerEvasion),
        Box::new(sandbox_system_checks::SandboxSystemChecks),
        Box::new(sandbox_time_evasion::SandboxTimeEvasion),
        Box::new(sandbox_user_activity::SandboxUserActivity),
        Box::new(anti_instrumentation::AntiInstrumentation),
        Box::new(self_deletion::SelfDeletion),
        Box::new(encoded_strings::EncodedStrings),
        Box::new(packing::Packing),
        Box::new(host_binding::HostBinding),
        Box::new(embedded_content::EmbeddedContent),
        Box::new(signature_anomaly::SignatureAnomaly),
    ]
}
