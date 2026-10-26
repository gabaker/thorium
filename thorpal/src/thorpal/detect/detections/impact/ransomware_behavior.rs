//! Ransomware behavior around encryption (MITRE ATT&CK **T1486**).
//!
//! Where `crypto_capability` proves a sample *can* encrypt, this flags the
//! surrounding ransomware workflow that turns crypto into impact:
//!
//! * **Target discovery** — enumerating drives (`A:`–`Z:`) and walking the
//!   filesystem to find victim files (every drive, recursing writable directories).
//! * **Rename / extension rewrite** — appending a marker extension to each
//!   encrypted file.
//! * **Ransom notes** — dropping a note with payment/recovery instructions.
//!
//! Individually these are weak; co-located with `crypto_capability` (the
//! validator's clustering bumps such groups) they are a strong ransomware signal.

use anyhow::Result;

use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, low, medium,
    string_artifact_hits,
};

use super::T1486;

/// The Ransomware behavior detection (ATT&CK T1486).
pub struct RansomwareBehavior;

/// Drive/file enumeration and rename APIs used to find and mark victim files.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetLogicalDriveStrings",
        severity: medium(),
        reasoning: "Calls GetLogicalDriveStrings to enumerate every mounted drive — the drive sweep \
                    a ransomware performs before encrypting files across all volumes.",
    },
    ApiSpec {
        key: "GetLogicalDrives",
        severity: low(),
        reasoning: "Calls GetLogicalDrives to list drive letters (A:-Z:), the first step of a \
                    ransomware volume sweep.",
    },
    ApiSpec {
        key: "FindFirstFile",
        severity: low(),
        reasoning: "Calls FindFirstFile to begin a directory walk; ransomware recurses the tree to \
                    find victim files to encrypt. Common, hence low confidence.",
    },
    ApiSpec {
        key: "FindNextFile",
        severity: low(),
        reasoning: "Calls FindNextFile to continue a directory walk over candidate victim files. \
                    Common, hence low confidence.",
    },
    ApiSpec {
        key: "GetDriveType",
        severity: low(),
        reasoning: "Calls GetDriveType to classify each enumerated drive (fixed/removable/network) — \
                    ransomware uses this to pick which volumes to encrypt.",
    },
    ApiSpec {
        key: "SetFileAttributes",
        severity: low(),
        reasoning: "Calls SetFileAttributes, e.g. to clear read-only before overwriting a file with \
                    ciphertext during encryption.",
    },
    ApiSpec {
        key: "MoveFile",
        severity: medium(),
        reasoning: "Calls MoveFile to rename files; ransomware renames each encrypted file to append \
                    a marker extension.",
    },
    ApiSpec {
        key: "GetVolumeInformation",
        severity: low(),
        reasoning: "Calls GetVolumeInformation to inspect a volume (name/serial/filesystem) while \
                    sweeping drives to encrypt.",
    },
];

/// Ransom-note / extortion phrases (distinctive enough for medium confidence).
const NOTE_STRINGS: &[&str] = &[
    "have been encrypted",
    "your files",
    "all your files",
    "how to decrypt",
    "how to recover",
    "recover your file",
    "decrypt your file",
    ".onion",
];

#[async_trait::async_trait]
impl Detection for RansomwareBehavior {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "ransomware_behavior"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1486
    }

    /// Run the enumeration/rename API and ransom-note string passes.
    #[tracing::instrument(name = "RansomwareBehavior::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();

        // 1) drive/file enumeration + rename API call sites
        hits.extend(api_call_hits(ctx, T1486, APIS, EvidenceKind::ContextOnly).await?);

        // 2) ransom-note / extortion phrases
        hits.extend(
            string_artifact_hits(
                ctx,
                T1486,
                NOTE_STRINGS,
                medium(),
                "Contains ransom-note / extortion wording (\"...have been encrypted\", \"how to \
                 decrypt/recover\", a .onion address). This is the victim-facing note a ransomware \
                 drops with payment and recovery instructions.",
                &[],
            )
            .await?,
        );

        Ok(hits)
    }
}
