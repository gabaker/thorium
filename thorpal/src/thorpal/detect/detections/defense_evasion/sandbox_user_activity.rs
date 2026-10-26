//! Virtualization/Sandbox Evasion — User Activity Based Checks (MITRE ATT&CK **T1497.002**).
//!
//! Detects malware that waits for signs of a real human before detonating, since
//! automated sandboxes usually have no mouse movement, keystrokes, or foreground
//! activity. If the environment looks unattended the payload stays dormant, so
//! the sandbox observes nothing.
//!
//! All indicators here are imported user-interaction APIs, reported at their call
//! sites: cursor position, mouse/keyboard state, idle time, input hooks,
//! foreground window, and screen resolution.

use anyhow::Result;

use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, high, low,
    medium,
};

use super::T1497_002;

/// The User Activity Based Checks detection (ATT&CK T1497.002).
pub struct SandboxUserActivity;

/// User-interaction APIs used to detect an unattended (sandbox) environment.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetCursorPos",
        severity: medium(),
        reasoning: "Samples the mouse cursor position (GetCursorPos); a cursor that never moves \
                    indicates an automated sandbox with no human, so the payload stays dormant. \
                    Requires real user activity to trigger — frustrating automated analysis.",
    },
    ApiSpec {
        key: "mouse_event",
        severity: medium(),
        reasoning: "Uses mouse_event; combined with cursor sampling, malware checks for genuine \
                    mouse movement before acting.",
    },
    ApiSpec {
        key: "GetAsyncKeyState",
        severity: medium(),
        reasoning: "Watches for keystrokes (GetAsyncKeyState); no keyboard activity implies an \
                    unattended sandbox, so the malware waits for genuine input before acting.",
    },
    ApiSpec {
        key: "GetKeyState",
        severity: medium(),
        reasoning: "Watches keyboard state (GetKeyState) to require real user input before \
                    detonating.",
    },
    ApiSpec {
        key: "GetLastInputInfo",
        severity: high(),
        reasoning: "Measures time since the last user input (GetLastInputInfo); a large idle time \
                    signals an unattended automated environment, so the malware delays or aborts. \
                    Rarely used by benign software for this purpose.",
    },
    ApiSpec {
        key: "SetWindowsHookEx",
        severity: medium(),
        reasoning: "Hooks mouse/keyboard input (SetWindowsHookEx) to wait for (or log) real user \
                    interaction before detonating, evading sandboxes that do not simulate input.",
    },
    ApiSpec {
        key: "GetForegroundWindow",
        severity: medium(),
        reasoning: "Checks the foreground window (GetForegroundWindow) to see whether a user is \
                    actively interacting; a static foreground suggests an automated host.",
    },
    ApiSpec {
        key: "GetSystemMetrics",
        severity: low(),
        reasoning: "Reads screen metrics (GetSystemMetrics), e.g. resolution; unusually small or \
                    default resolutions are characteristic of headless sandboxes.",
    },
    ApiSpec {
        key: "EnumDisplaySettings",
        severity: low(),
        reasoning: "Enumerates display settings (EnumDisplaySettings) to spot the low/default \
                    resolutions typical of headless sandboxes.",
    },
];

#[async_trait::async_trait]
impl Detection for SandboxUserActivity {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "sandbox_user_activity"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1497_002
    }

    /// Report call sites of user-interaction APIs used for sandbox evasion.
    #[tracing::instrument(name = "SandboxUserActivity::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // every indicator here is an imported API whose result gates behavior
        api_call_hits(ctx, T1497_002, APIS, EvidenceKind::ApiResultCheck).await
    }
}
