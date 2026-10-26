//! MITRE ATT&CK tactic: **Command and Control** (TA0011).
//!
//! Detections here flag how a sample *communicates with a remote system* — the
//! network side of malware an analyst wants to characterize first: does it talk to
//! the network, over what protocol, and to where. Today this is one detection family
//! spanning several closely-related techniques:
//!
//! * **T1071** — Application Layer Protocol: the parent technique, used for
//!   QUIC/HTTP-3 libraries that don't slot neatly under the web/DNS sub-techniques.
//! * **T1071.001** — Application Layer Protocol: Web Protocols (HTTP/HTTPS,
//!   WebSockets) — WinINet/WinHTTP/urlmon/libcurl, and hardcoded URLs / user-agents.
//! * **T1071.004** — Application Layer Protocol: DNS — resolver APIs.
//! * **T1095** — Non-Application Layer Protocol: raw TCP/UDP sockets (Winsock + libc)
//!   and recovered host:port / IP IOCs.
//! * **T1571** — Non-Standard Port: a hardcoded non-standard port recovered from
//!   socket-setup code (`htons`).
//! * **T1573** — Encrypted Channel: TLS/SSL libraries (OpenSSL/mbedTLS/SChannel).
//!
//! The whole family is implemented by a single [`network_comms::NetworkComms`]
//! detection (mirroring how `impact/crypto_capability` is one detection with several
//! sub-passes), which emits hits across all of the techniques above. [`ioc_pools`] is
//! a sub-pass of it rather than a detection of its own: it groups the indicators
//! `network_comms` recovered and reports the structure they were stored and referenced
//! in.

pub mod ioc_pools;
pub mod network_comms;

use std::borrow::Cow;

use crate::thorpal::detect::detections::{AttackTactic, AttackTechnique, Detection};

/// ATT&CK T1071 — Application Layer Protocol (the parent technique).
pub const T1071: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1071"),
    name: Cow::Borrowed("Application Layer Protocol"),
    tactic: AttackTactic::CommandAndControl,
};

/// ATT&CK T1071.001 — Application Layer Protocol: Web Protocols.
pub const T1071_001: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1071.001"),
    name: Cow::Borrowed("Application Layer Protocol: Web Protocols"),
    tactic: AttackTactic::CommandAndControl,
};

/// ATT&CK T1071.004 — Application Layer Protocol: DNS.
pub const T1071_004: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1071.004"),
    name: Cow::Borrowed("Application Layer Protocol: DNS"),
    tactic: AttackTactic::CommandAndControl,
};

/// ATT&CK T1095 — Non-Application Layer Protocol (raw TCP/UDP sockets).
pub const T1095: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1095"),
    name: Cow::Borrowed("Non-Application Layer Protocol"),
    tactic: AttackTactic::CommandAndControl,
};

/// ATT&CK T1571 — Non-Standard Port.
pub const T1571: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1571"),
    name: Cow::Borrowed("Non-Standard Port"),
    tactic: AttackTactic::CommandAndControl,
};

/// ATT&CK T1573 — Encrypted Channel (TLS/SSL).
pub const T1573: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1573"),
    name: Cow::Borrowed("Encrypted Channel"),
    tactic: AttackTactic::CommandAndControl,
};

/// The Command and Control detections, instantiated for the registry.
pub fn detections() -> Vec<Box<dyn Detection>> {
    // one detection struct per technique-family module
    vec![Box::new(network_comms::NetworkComms)]
}
