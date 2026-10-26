//! MITRE ATT&CK tactic: **Execution** (TA0002).
//!
//! Detections here flag how malware *runs attacker-controlled code* in ways that
//! also evade monitoring. Today this is one technique:
//!
//! * **T1106** — Native API: reaching the kernel through the lowest-level
//!   interface available (`ntdll` `Nt*`/`Zw*` functions, or a raw syscall) instead
//!   of the documented Win32 APIs, so behavior slips under the user-mode API hooks
//!   an EDR places at the Win32 layer.
//!
//! Additional Execution techniques (Command and Scripting Interpreter, Shared
//! Modules, …) are added as sibling modules here.

pub mod native_api;

use std::borrow::Cow;

use crate::thorpal::detect::detections::{AttackTactic, AttackTechnique, Detection};

/// ATT&CK T1106 — Native API.
pub const T1106: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1106"),
    name: Cow::Borrowed("Native API"),
    tactic: AttackTactic::Execution,
};

/// The Execution detections, instantiated for the registry.
pub fn detections() -> Vec<Box<dyn Detection>> {
    // one detection struct per technique-family module
    vec![Box::new(native_api::NativeApi)]
}
