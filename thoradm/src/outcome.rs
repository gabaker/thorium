//! How a run ended, and the process exit code it becomes

use crate::backup::failures::FailureLog;

/// How a backup, restore, verify, or scrub run ended; its value is the process exit code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RunOutcome {
    /// Every object was handled and nothing was recorded
    Complete = 0,
    /// The run stopped early: the first record under `--abort-on-failure`, a preflight refusal, a
    /// usage or configuration error, a dead step, or a failure that isn't per object
    Aborted = 1,
    /// The run reached its end and `failures.jsonl` lists every failed or skipped object
    CompletedWithFailures = 2,
}

impl RunOutcome {
    /// Derives the outcome of a run whose workers have all returned from its failure log
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log
    #[must_use]
    pub fn finished(log: &FailureLog) -> Self {
        // a run that --abort-on-failure stopped is an abort, whatever it finished before stopping
        if log.stopped() {
            return RunOutcome::Aborted;
        }
        // any failed or skipped record means the run wasn't complete
        if log.total() > 0 {
            RunOutcome::CompletedWithFailures
        } else {
            RunOutcome::Complete
        }
    }

    /// Returns the process exit code for this outcome
    #[must_use]
    pub fn code(self) -> u8 {
        // the discriminant is the documented exit code
        self as u8
    }
}

impl From<RunOutcome> for std::process::ExitCode {
    /// Converts an outcome to the process exit code
    ///
    /// # Arguments
    ///
    /// * `outcome` - The run's outcome
    fn from(outcome: RunOutcome) -> Self {
        // the discriminant is the documented exit code
        std::process::ExitCode::from(outcome.code())
    }
}
