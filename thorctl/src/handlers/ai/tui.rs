//! Shared terminal plumbing for thorctl's AI TUIs
//!
//! The chat TUI and the standalone usage TUI bracket the terminal identically:
//! color-eyre for readable panics, mouse capture and bracketed paste around
//! `ratatui`'s raw-mode/alternate-screen init, and the mirrored teardown. The
//! event forwarders and the file-only tracing subscriber live here too, so a
//! second TUI cannot drift from the chat's hard-learned sequences.

use std::io::stdout;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream,
};
use crossterm::execute;
use futures::stream::StreamExt;
use kanal::AsyncSender;
use ratatui::DefaultTerminal;

/// Take the terminal for a TUI: eyre hooks, mouse + paste capture, raw mode
///
/// The caller owns the screen from here until [`restore_terminal`]; nothing
/// may print to stdout/stderr in between (tracing goes to the file subscriber
/// [`init_tracing_log`] installs).
pub(super) fn init_terminal() -> std::io::Result<DefaultTerminal> {
    // setup eyre so it can help us have nice errors; a second install (another
    // TUI already ran in-process) is harmless and skipped
    let _ = color_eyre::install();
    // start capturing mouse clicks, and bracketed pastes so a paste arrives
    // as one event (not a key stream whose newlines submit a prompt early)
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    // initialize the terminal (raw mode, alternate screen, panic hooks)
    Ok(ratatui::init())
}

/// Give the terminal back: leave raw mode, drop mouse + paste capture
///
/// Best-effort by design: teardown must keep going even when the terminal is
/// already gone (an ssh drop), so a failure is reported on stderr -- safe
/// again once ratatui restored the screen -- rather than returned.
pub(super) fn restore_terminal() {
    // restore the terminal to its original state
    ratatui::restore();
    // stop capturing mouse clicks and bracketed pastes, best-effort: a failure
    // here must not skip whatever teardown the caller still has to run
    if let Err(err) = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste) {
        eprintln!("warning: failed to disable mouse capture: {err}");
    }
}

/// Install a TUI's tracing subscriber: a file log plus optional log capture
///
/// A TUI owns the terminal, so tracing must never write to stdout/stderr;
/// instead every `tracing` line (including ghidra-cli's `[Ghidra import
/// stdout]` output, which carries analyzeHeadless's real failure reasons)
/// lands in `~/.thorium/logs/thorctl-ai.log`. The file layer's filter honors
/// `RUST_LOG` and defaults to `info`, exactly as before. The capture layer,
/// when one is passed (the chat TUI streams tool-call log lines through it;
/// the usage TUI has no live session and passes `None`), carries its own
/// static target filter, so a `RUST_LOG=warn` thins only the file log and
/// never starves the TUI's live log window. Installation is best-effort: an
/// unresolvable log file installs just the capture layer, and a subscriber
/// installed elsewhere silently skips setup rather than failing the TUI.
///
/// # Arguments
///
/// * `tool_logs` - The capture layer routing tool-call log lines to the TUI
pub(super) fn init_tracing_log(tool_logs: Option<thorpal::ToolLogLayer>) {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // scope the capture layer to the streamed targets so idle runs never pay
    // event-dispatch cost and a strict RUST_LOG cannot silence the TUI stream
    let tool_logs =
        tool_logs.map(|layer| layer.with_filter(thorpal::ToolLogRouter::targets_filter()));
    // resolve the log file under the user's .thorium home, best-effort
    let file = dirs::home_dir().and_then(|home| {
        let dir = home.join(".thorium").join("logs");
        // create the directory, skipping the file log when that fails
        std::fs::create_dir_all(&dir).ok()?;
        // open the log file in append mode so runs accumulate in one place
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(dir.join("thorctl-ai.log"))
            .ok()
    });
    // build the file layer when the log file resolved; honor an explicit
    // RUST_LOG for the file log only, defaulting to info-level everywhere
    let fmt_layer = file.map(|file| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        tracing_subscriber::fmt::layer()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_filter(filter)
    });
    // install the composed registry, ignoring an already-set global; the file
    // layer is optional so capture still works under a read-only ~/.thorium
    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(tool_logs)
        .try_init();
}

/// Background task that forwards terminal events to the central event channel
///
/// # Arguments
///
/// * `event_tx` - The app event channel terminal events are forwarded over
/// * `map` - Wraps a terminal event in the app's own event type
async fn terminal_event_forwarder<E, M>(event_tx: AsyncSender<E>, map: M)
where
    E: Send + 'static,
    M: Fn(Event) -> E + Send + 'static,
{
    // build a stream of raw terminal events to forward
    let mut event_stream = EventStream::new();
    // forward each event until the stream or the app's channel ends
    while let Some(event_result) = event_stream.next().await {
        match event_result {
            Ok(event) => {
                if event_tx.send(map(event)).await.is_err() {
                    // the channel closed: the app is tearing down, stop
                    break;
                }
            }
            Err(_) => {
                // terminal event errors are transient; keep polling
                continue;
            }
        }
    }
}

/// Background task that turns SIGTERMs into a graceful quit, then a force-quit
///
/// An external `kill` otherwise ends the process mid-write: the session's
/// final flush never runs and the Ghidra bridge is orphaned holding the
/// project lock. The first signal is forwarded as the app's quit event so it
/// routes through the same teardown the user's own quit takes; when the event
/// channel is already closed (teardown underway) that is said out loud instead
/// of silently dropping the signal. Any later SIGTERM force-quits with the
/// conventional exit code 130 (headless second-signal parity), so a wedged
/// teardown can always be killed.
///
/// # Arguments
///
/// * `event_tx` - The app event channel to send the quit event over
/// * `quit` - Builds the app's graceful-quit event
#[cfg(unix)]
async fn sigterm_forwarder<E, Q>(event_tx: AsyncSender<E>, quit: Q)
where
    E: Send + 'static,
    Q: Fn() -> E + Send + 'static,
{
    // failing to register just skips graceful handling rather than erroring
    let Ok(mut sigterm) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        return;
    };
    // whether a graceful quit was already requested; the next signal escalates
    let mut quit_requested = false;
    // keep listening so repeated kills escalate instead of vanishing
    while sigterm.recv().await.is_some() {
        // any signal after the first quit request force-quits immediately
        if quit_requested {
            eprintln!("thorctl: force-quitting on a repeated SIGTERM");
            std::process::exit(130);
        }
        quit_requested = true;
        // the first signal is one quit request routed through the app's loop
        if event_tx.send(quit()).await.is_err() {
            // the channel is closed: teardown is already underway, so say how
            // to get out instead of silently dropping the signal
            eprintln!("thorctl: teardown in progress; signal again to force-quit");
        }
    }
}

/// Spawn the core event forwarders for a TUI's event loop
///
/// # Arguments
///
/// * `sender` - The app event channel the forwarders feed
/// * `terminal` - Wraps a terminal event in the app's own event type
/// * `quit` - Builds the app's graceful-quit event for a SIGTERM
pub(super) fn spawn_forwarders<E, T, Q>(sender: &AsyncSender<E>, terminal: T, quit: Q)
where
    E: Send + 'static,
    T: Fn(Event) -> E + Send + 'static,
    Q: Fn() -> E + Send + 'static,
{
    // spawn our terminal event forwarder
    tokio::spawn(terminal_event_forwarder(sender.clone(), terminal));
    // turn an external SIGTERM into the same graceful quit the user can take
    #[cfg(unix)]
    tokio::spawn(sigterm_forwarder(sender.clone(), quit));
    // non-unix builds have no SIGTERM to forward; the quit factory goes unused
    #[cfg(not(unix))]
    drop(quit);
}

/// Sends a redraw event once a second so relative figures stay fresh
///
/// The shared slow ticker behind the telemetry TUIs: none of them animates
/// anything faster than once a second, and a send failure just means the app
/// is gone, so the loop quietly ends with it.
///
/// # Arguments
///
/// * `sender` - The channel to request redraws over
/// * `redraw` - Builds the consumer's redraw event
pub(super) async fn redraw_ticker<E, R>(sender: AsyncSender<E>, redraw: R)
where
    E: Send + 'static,
    R: Fn() -> E + Send + 'static,
{
    // keep ticking for the life of the app
    loop {
        // sleep for 1s between frames; these TUIs animate nothing faster
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // try to send a redraw event; a failure just means the app is gone
        if sender.send(redraw()).await.is_err() {
            return;
        }
    }
}
