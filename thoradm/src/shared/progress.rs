//! The progress bars and stderr log every thoradm command shares
//!
//! All progress bars hang off one `MultiProgress` that draws only when stderr is a terminal.
//! Log lines go through `tracing` to stderr in every case; on a terminal each line is printed
//! with the bars suspended, so bars never overwrite it. The bars carry no message that isn't
//! also logged.

use indicatif::{MultiProgress, ProgressDrawTarget};
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;
use tracing_subscriber::EnvFilter;

/// The bars every controller adds its progress bars to
static BARS: OnceLock<MultiProgress> = OnceLock::new();

/// Returns the shared progress bars, drawn only when stderr is a terminal
pub fn bars() -> &'static MultiProgress {
    // build the bars once, hidden when nobody is watching stderr
    BARS.get_or_init(|| {
        let target = if std::io::stderr().is_terminal() {
            ProgressDrawTarget::stderr()
        } else {
            ProgressDrawTarget::hidden()
        };
        MultiProgress::with_draw_target(target)
    })
}

/// A stderr writer that suspends the progress bars while it writes
#[derive(Debug, Clone, Copy, Default)]
pub struct BarAwareStderr;

impl Write for BarAwareStderr {
    /// Writes bytes to stderr with the progress bars suspended
    ///
    /// # Arguments
    ///
    /// * `buf` - The bytes to write
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // hide the bars, write the whole line, and redraw them
        bars().suspend(|| std::io::stderr().write_all(buf))?;
        Ok(buf.len())
    }

    /// Flushes stderr
    fn flush(&mut self) -> std::io::Result<()> {
        // stderr is unbuffered, but flush it anyway
        std::io::stderr().flush()
    }
}

/// Sends every log event to stderr, filtered by `RUST_LOG` (default `info`)
///
/// Calling it more than once keeps the first subscriber.
pub fn init_logging() {
    // honor RUST_LOG-style filters, logging info and above by default
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // write each event as one line to stderr, colored only on a terminal
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(|| BarAwareStderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .try_init();
}
