//! The short summary a command leaves where Kubernetes reads a container's termination message,
//! and the Job a command runs in

use std::path::{Path, PathBuf};
use tracing::warn;

/// The environment variable naming another file to write the termination summary to
pub const TERMINATION_LOG_ENV: &str = "THORIUM_TERMINATION_LOG";

/// Where Kubernetes reads a container's termination message by default
const TERMINATION_LOG: &str = "/dev/termination-log";

/// The most bytes of a termination message Kubernetes keeps
pub const TERMINATION_LIMIT: usize = 4096;

/// The environment variable holding the Job's UID, which keys the Job's result row
pub const JOB_UID_ENV: &str = "THORIUM_JOB_UID";

/// Get the UID of the Job this process runs in, when it runs in one
#[must_use]
pub fn job_uid() -> Option<String> {
    // the pod's label, passed in by the Job's spec; a run by hand has none
    std::env::var(JOB_UID_ENV)
        .ok()
        .filter(|uid| !uid.trim().is_empty())
}

/// Write the summary where Kubernetes reads the termination message, when there is such a place
///
/// # Arguments
///
/// * `summary` - The summary to write
pub fn write_summary(summary: &str) {
    // an explicit file wins; otherwise only the file Kubernetes provides is written
    let path = std::env::var_os(TERMINATION_LOG_ENV)
        .map(PathBuf::from)
        .or_else(|| {
            Path::new(TERMINATION_LOG)
                .exists()
                .then(|| TERMINATION_LOG.into())
        });
    if let Some(path) = path
        && let Err(err) = std::fs::write(&path, summary)
    {
        warn!(
            "Failed to write the termination summary to {}: {err}",
            path.display()
        );
    }
}
