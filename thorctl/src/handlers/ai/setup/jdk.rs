//! The JDK probe behind the setup wizard's Java check
//!
//! Ghidra compiles its bridge script at runtime, so it needs a full JDK (javac
//! plus the `jdk.compiler` module), not a JRE. The probing itself lives in
//! ghidra-cli's library-exported `ghidra::java` module -- the exact code
//! ThorPal's bridge resolves Java with at analysis time -- so what the wizard
//! reports is what a real run will find.

use std::path::{Path, PathBuf};

use ghidra_cli::ghidra::java;

/// A JDK probe outcome the wizard renders
#[derive(Debug, Clone)]
pub struct JdkReport {
    /// Whether a usable full JDK was found
    pub ok: bool,
    /// The human-readable summary: where the JDK is, or how to fix the miss
    pub summary: String,
}

/// Probe for the full JDK a Ghidra install needs, off the event loop
///
/// The probe shells out to `java -version`/`--list-modules`, so it runs on the
/// blocking pool. With an install in hand the required major version comes
/// from that install's `application.properties`; without one the documented
/// default floor applies.
///
/// # Arguments
///
/// * `install` - The chosen Ghidra install, when one has been picked
pub async fn check_jdk(install: Option<PathBuf>) -> JdkReport {
    // the probe is blocking subprocess work, so keep it off the event loop
    let result = tokio::task::spawn_blocking(move || probe(install.as_deref())).await;
    match result {
        Ok(report) => report,
        // a panicked probe still renders as a failed check, never a crash
        Err(error) => JdkReport {
            ok: false,
            summary: format!("the JDK probe failed unexpectedly: {error}"),
        },
    }
}

/// Run the blocking JDK resolution and fold it into a report
///
/// # Arguments
///
/// * `install` - The chosen Ghidra install, when one has been picked
fn probe(install: Option<&Path>) -> JdkReport {
    match install {
        // with an install, resolve against its real minimum and get the
        // per-OS fix-it text on a miss
        Some(install) => match java::resolve_for_ghidra(install, None) {
            Ok(info) => JdkReport {
                ok: true,
                summary: format!(
                    "JDK {} at {} (via {})",
                    info.major,
                    info.home.display(),
                    info.source
                ),
            },
            Err(error) => JdkReport {
                ok: false,
                summary: error,
            },
        },
        // without one, probe against the documented default floor
        None => {
            let status = java::resolve_jdk(None, java::DEFAULT_MIN_JAVA);
            match &status {
                java::JavaStatus::Ok(info) => JdkReport {
                    ok: true,
                    summary: format!(
                        "JDK {} at {} (via {})",
                        info.major,
                        info.home.display(),
                        info.source
                    ),
                },
                other => JdkReport {
                    ok: false,
                    summary: java::describe_failure(other),
                },
            }
        }
    }
}
