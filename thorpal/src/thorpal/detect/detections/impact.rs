//! MITRE ATT&CK tactic: **Impact** (TA0040).
//!
//! Detections here flag capabilities that manipulate, interrupt, or destroy
//! systems and data. Today, both center on ransomware:
//!
//! * **T1486** (Data Encrypted for Impact) — the *crypto capability* itself
//!   (crypto constants, embedded keys, crypto APIs) and the *ransomware behavior*
//!   around it (drive/file enumeration, rename/extension rewrite, ransom notes).
//!
//! Additional Impact techniques (T1490 Inhibit System Recovery, T1489 Service
//! Stop, …) are added as sibling modules here.

pub mod crypto_capability;
pub mod ransomware_behavior;

use std::borrow::Cow;

use crate::thorpal::detect::detections::{AttackTactic, AttackTechnique, Detection};

/// ATT&CK T1486 — Data Encrypted for Impact.
pub const T1486: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1486"),
    name: Cow::Borrowed("Data Encrypted for Impact"),
    tactic: AttackTactic::Impact,
};

/// The Impact detections, instantiated for the registry.
pub fn detections() -> Vec<Box<dyn Detection>> {
    // one detection struct per technique-family module
    vec![
        Box::new(crypto_capability::CryptoCapability),
        Box::new(ransomware_behavior::RansomwareBehavior),
    ]
}
