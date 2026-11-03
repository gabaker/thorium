//! A timer for tests that bound how long an operation takes, which counts the processor time the
//! test's thread uses so the time it spends waiting for a processor on a loaded machine doesn't
//! count against the bound

use std::time::{Duration, Instant};

/// Times an operation run on the current thread
///
/// On Linux it counts the processor time the thread uses (its user and system time), which the
/// thread waiting for a processor doesn't add to. Elsewhere, or when the thread's statistics
/// can't be read, it counts wall-clock time.
pub(crate) struct ThreadTimer {
    /// The thread's processor time when the timer started, if it could be read
    cpu: Option<Duration>,
    /// The wall-clock time when the timer started
    wall: Instant,
}

impl ThreadTimer {
    /// Start timing on the current thread
    #[must_use]
    pub(crate) fn start() -> Self {
        // read both clocks so the wall clock can stand in if the thread's time can't be read
        ThreadTimer {
            cpu: thread_cpu_time(),
            wall: Instant::now(),
        }
    }

    /// Get the time the current thread has used since the timer started
    #[must_use]
    pub(crate) fn elapsed(&self) -> Duration {
        // use the thread's processor time when it could be read both times
        match (self.cpu, thread_cpu_time()) {
            (Some(start), Some(now)) => now.saturating_sub(start),
            _ => self.wall.elapsed(),
        }
    }
}

/// Read the processor time the current thread has used
///
/// Linux reports a thread's user and system time in `/proc/thread-self/stat` in clock ticks,
/// which are a hundredth of a second (`USER_HZ`) for every program.
fn thread_cpu_time() -> Option<Duration> {
    // read this thread's statistics
    let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
    // the fields after the parenthesized thread name start at the state (field 3), so the user
    // and system times (fields 14 and 15) follow the first 11 of them
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_whitespace().skip(11);
    let user = fields.next()?.parse::<u64>().ok()?;
    let system = fields.next()?.parse::<u64>().ok()?;
    // convert the ticks to a duration
    Some(Duration::from_millis((user + system) * 10))
}
