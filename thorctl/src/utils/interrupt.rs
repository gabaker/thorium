//! Tidy up the terminal and the temporary files when Thorctl is interrupted
//!
//! A prompt hides the cursor and a password prompt turns echo off; killing the process in the
//! middle (Ctrl-C, `kill`, a closed terminal) would leave the shell with an invisible cursor or
//! a terminal that does not echo what is typed. An edit session also keeps the resource it is
//! editing in a private temporary directory, which would stay behind. [`install`] catches
//! `SIGINT`, `SIGTERM` and `SIGHUP`, removes the tracked paths, puts the terminal back as it
//! found it, and then dies from the same signal so the exit status is unchanged.
//!
//! While an editor is open ([`while_editing`]) `SIGINT` is left to the editor, as `git` does: a
//! Ctrl-C reaches the editor and Thorctl together, and the editor decides what it means.

use std::path::{Path, PathBuf};

/// The paths removed when a fatal signal arrives
static TRACKED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// How many editors are open right now; `SIGINT` is ignored while any is
static EDITORS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A temporary path that is removed if the process is interrupted while the guard is alive
pub struct Tracked {
    /// The path to remove on interruption
    path: PathBuf,
}

impl Drop for Tracked {
    /// Stop tracking the path, since its owner cleans it up
    fn drop(&mut self) {
        // forget this path so a later signal does not touch it
        if let Ok(mut tracked) = TRACKED.lock() {
            tracked.retain(|path| path != &self.path);
        }
    }
}

/// Remove `path` (a file or a directory) if the process is interrupted while the guard lives
///
/// # Arguments
///
/// * `path` - The temporary file or directory to clean up on interruption
#[must_use]
pub fn track(path: &Path) -> Tracked {
    // remember the path for the signal thread
    if let Ok(mut tracked) = TRACKED.lock() {
        tracked.push(path.to_path_buf());
    }
    Tracked {
        path: path.to_path_buf(),
    }
}

/// A guard that leaves `SIGINT` to an editor while it is alive
pub struct Editing;

impl Drop for Editing {
    /// Take `SIGINT` back from the editor
    fn drop(&mut self) {
        // one editor fewer
        EDITORS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Leave `SIGINT` to the editor that is about to open, until the guard drops
#[must_use]
pub fn while_editing() -> Editing {
    // one editor more
    EDITORS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Editing
}

/// End the process the way Ctrl-C would, for a prompt that read the interrupt as a key press
///
/// A password prompt in raw mode reports Ctrl-C as an error instead of raising `SIGINT`; this
/// puts the terminal back and exits with the usual interrupted status (130).
pub fn exit_interrupted() -> ! {
    // restore the terminal and drop the temporary files, as for a real signal
    #[cfg(unix)]
    unix::cleanup();
    std::process::exit(130)
}

/// Whether an error is a prompt that ended on Ctrl-C read as a key press
///
/// A prompt that has put the terminal in raw mode reports Ctrl-C as an interrupted read instead
/// of raising `SIGINT`, and the command that asked surfaces it as an error.
///
/// # Arguments
///
/// * `error` - The error a command ended with
#[must_use]
pub fn was_interrupted(error: &thorium::Error) -> bool {
    match error {
        // dialoguer's own error carries the kind of the interrupted read
        thorium::Error::Dialoguer(dialoguer::Error::IO(io)) => {
            io.kind() == std::io::ErrorKind::Interrupted
        }
        // prompts that wrap the error in a message keep the reader's wording
        thorium::Error::Generic(message) => message.contains("read interrupted"),
        _ => false,
    }
}

/// Show the cursor and put the terminal back as it was at startup
///
/// A prompt that ends in an error or on a key press does not always give the cursor back, so
/// this is safe to call at any time: it only writes to a terminal that can take the sequence.
pub fn restore_terminal() {
    // only unix has the terminal settings to restore
    #[cfg(unix)]
    unix::restore_terminal();
}

/// Start catching `SIGINT`, `SIGTERM` and `SIGHUP` (a no-op where signals do not exist)
pub fn install() {
    // only unix has these signals
    #[cfg(unix)]
    unix::install();
}

/// The unix implementation: a handler that writes the signal number to a pipe and a thread that
/// does the real work, since almost nothing is safe to call inside a signal handler
#[cfg(unix)]
mod unix {
    use std::io::Write;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// The write end of the pipe the handler writes to
    static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

    /// The terminal settings of stdin when the process started, if stdin is a terminal
    static TERMINAL: OnceLock<Option<libc::termios>> = OnceLock::new();

    /// The signal handler: pass the signal number to the thread over the pipe
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal that arrived
    extern "C" fn on_signal(signal: libc::c_int) {
        // a signal number fits in a byte
        let byte = [u8::try_from(signal).unwrap_or(0)];
        // SAFETY: `write` is async-signal-safe and the buffer is a live one-byte array
        unsafe {
            libc::write(PIPE_WRITE.load(Ordering::Relaxed), byte.as_ptr().cast(), 1);
        }
    }

    /// Read the terminal settings of stdin
    fn read_terminal() -> Option<libc::termios> {
        // SAFETY: `isatty` takes any descriptor
        if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
            return None;
        }
        // SAFETY: an all-zero termios is a valid value for `tcgetattr` to fill
        let mut settings: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: the pointer refers to a live termios
        let status = unsafe { libc::tcgetattr(libc::STDIN_FILENO, &raw mut settings) };
        (status == 0).then_some(settings)
    }

    /// Undo what an interrupted prompt leaves behind: a hidden cursor and a silent terminal
    pub fn restore_terminal() {
        // bring the cursor back when something is drawing on a terminal that understands it
        let dumb = std::env::var_os("TERM").is_some_and(|term| term == "dumb");
        if std::io::IsTerminal::is_terminal(&std::io::stderr()) && !dumb {
            let _ = std::io::stderr().write_all(b"\x1b[?25h");
        }
        // put stdin's terminal settings back (echo, line mode) as they were at startup
        if let Some(Some(settings)) = TERMINAL.get() {
            // SAFETY: the pointer refers to the saved, live termios
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, settings);
            }
        }
    }

    /// Remove the tracked temporary files and leave the terminal usable
    pub fn cleanup() {
        // remove the temporary files (taking them out of the list so nothing else touches them)
        let paths = match super::TRACKED.lock() {
            Ok(mut tracked) => std::mem::take(&mut *tracked),
            Err(_) => Vec::new(),
        };
        for path in paths {
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
        // leave the terminal usable
        restore_terminal();
    }

    /// Handle one signal: clean up, then die from it
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal that arrived
    fn handle(signal: libc::c_int) {
        // an editor that is open gets SIGINT; Thorctl keeps waiting for it
        if signal == libc::SIGINT && super::EDITORS.load(Ordering::SeqCst) > 0 {
            return;
        }
        // clean up before dying
        cleanup();
        // die from the signal itself so the parent sees the usual status
        // SAFETY: restoring the default action and re-raising the signal are plain libc calls
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
    }

    /// Install the handlers and start the thread that serves them
    pub fn install() {
        // remember how the terminal looks now
        TERMINAL.get_or_init(read_terminal);
        // make the pipe the handler talks to
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: the pointer refers to an array of two ints
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        // children (editors, container runtimes) must not inherit the pipe
        for fd in fds {
            // SAFETY: `fd` is a descriptor this function just created
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        PIPE_WRITE.store(fds[1], Ordering::Relaxed);
        // serve signals on a thread of their own
        let read_fd = fds[0];
        let spawned = std::thread::Builder::new()
            .name("thorctl-interrupt".to_owned())
            .spawn(move || {
                // read one signal number at a time for as long as the process lives
                let mut byte = [0u8; 1];
                loop {
                    // SAFETY: the buffer is a live one-byte array
                    let count = unsafe { libc::read(read_fd, byte.as_mut_ptr().cast(), 1) };
                    if count == 1 {
                        handle(libc::c_int::from(byte[0]));
                    } else if count == 0
                        || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                    {
                        return;
                    }
                }
            });
        if spawned.is_err() {
            return;
        }
        // catch the three signals that end an interactive session, except those the parent asked
        // to have ignored (a job started with `&` or `nohup` must outlive its terminal)
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: an all-zero sigaction is valid; the handler only calls `write`
            unsafe {
                let mut current: libc::sigaction = std::mem::zeroed();
                libc::sigaction(signal, std::ptr::null(), &raw mut current);
                if current.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&raw mut action.sa_mask);
                libc::sigaction(signal, &raw const action, std::ptr::null_mut());
            }
        }
    }
}
