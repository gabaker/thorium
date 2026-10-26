//! Self-deletion / Indicator Removal: File Deletion (MITRE ATT&CK **T1070.004**).
//!
//! Detects malware that erases itself after running so an analyst cannot recover
//! the sample from an infected host. Two tells:
//!
//! * **Overwrite-then-delete command lines** — the anti-forensics idiom of zeroing
//!   or randomizing the file's bytes *before* unlinking it, so the on-disk content
//!   cannot be carved back — Windows `fsutil file setZeroData … & Del /f /q <self>`,
//!   Linux `dd if=/dev/urandom of=<self> … && rm <self>`. Found via defined-string
//!   artifacts.
//! * **Self-delete APIs** — `MoveFileEx` with `MOVEFILE_DELAY_UNTIL_REBOOT` (delete
//!   on next boot), `DeleteFile`, and the command/exec sinks (`ShellExecute`,
//!   `WinExec`, `system`, …) used to launch a shell one-liner that deletes the
//!   malware's own path (usually paired with `GetModuleFileName`/`/proc/self/exe`).

use anyhow::Result;

use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, low, medium,
    string_artifact_hits,
};

use super::T1070_004;

/// The Self-deletion / Indicator Removal detection (ATT&CK T1070.004).
pub struct SelfDeletion;

/// Overwrite-before-delete artifact strings (the strong anti-forensics tell).
const OVERWRITE_STRINGS: &[&str] = &["setzerodata", "dd if=/dev/urandom", "shred -"];

/// Self-delete command-line artifact strings (shell one-liners that unlink self).
const DELETE_CMD_STRINGS: &[&str] = &["del /f", "/f /q", "&& rm ", "rm -f ", "rm -rf "];

/// APIs used to delete the sample itself or launch a self-delete command.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "MoveFileEx",
        severity: medium(),
        reasoning: "Calls MoveFileEx; with MOVEFILE_DELAY_UNTIL_REBOOT and a NULL destination this \
                    schedules the file (often the malware itself) for deletion on the next reboot — a \
                    classic self-removal / indicator-removal trick.",
    },
    ApiSpec {
        key: "DeleteFile",
        severity: low(),
        reasoning: "Calls DeleteFile; when the target is the malware's own path (from \
                    GetModuleFileName) this removes the sample from disk to frustrate recovery.",
    },
    ApiSpec {
        key: "GetModuleFileName",
        severity: low(),
        reasoning: "Calls GetModuleFileName to obtain its own executable path — the value a \
                    self-delete routine feeds to DeleteFile/MoveFileEx or a shell delete command.",
    },
    ApiSpec {
        key: "ShellExecute",
        severity: low(),
        reasoning: "Calls ShellExecute; malware uses it to launch a detached shell one-liner \
                    (cmd/ping/del or sleep/rm) that deletes the malware's own file after it exits.",
    },
    ApiSpec {
        key: "ShellExecuteEx",
        severity: low(),
        reasoning: "Calls ShellExecuteEx; used to spawn the shell one-liner that deletes the \
                    malware's own file after it exits.",
    },
    ApiSpec {
        key: "WinExec",
        severity: low(),
        reasoning: "Calls WinExec; a compact way to run the cmd self-delete one-liner (e.g. \
                    'cmd /c ... & del <self>') after execution.",
    },
    ApiSpec {
        key: "system",
        severity: low(),
        reasoning: "Calls system(); on Linux malware uses it to run the self-delete one-liner \
                    (e.g. 'sleep 5 && dd if=/dev/urandom of=<self> && rm <self>').",
    },
];

#[async_trait::async_trait]
impl Detection for SelfDeletion {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "self_deletion"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1070_004
    }

    /// Run the self-deletion string + API passes and merge their hits.
    #[tracing::instrument(name = "SelfDeletion::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();

        // 1) overwrite-before-delete command fragments (strong anti-forensics tell)
        hits.extend(
            string_artifact_hits(
                ctx,
                T1070_004,
                OVERWRITE_STRINGS,
                medium(),
                "Contains an overwrite-before-delete artifact (fsutil setZeroData / dd \
                 if=/dev/urandom / shred). Malware zeroes or randomizes its own bytes before \
                 unlinking so the sample cannot be carved back from disk.",
                &[],
            )
            .await?,
        );

        // 2) self-delete shell one-liners (del /f /q, rm -f, ...)
        hits.extend(
            string_artifact_hits(
                ctx,
                T1070_004,
                DELETE_CMD_STRINGS,
                medium(),
                "Contains a file-deletion shell command (Windows 'del /f /q' or Linux 'rm -f'); \
                 malware runs these against its own path to remove the sample after execution.",
                &[],
            )
            .await?,
        );

        // 3) self-delete / delayed-delete APIs
        hits.extend(api_call_hits(ctx, T1070_004, APIS, EvidenceKind::ContextOnly).await?);

        Ok(hits)
    }
}
