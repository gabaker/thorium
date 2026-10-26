//! Helpers for testing commands that open an editor or prompt on a terminal
//!
//! [`EditorScript`] builds a shell script to pass as `--editor`: each time thorctl opens the
//! editor the script rewrites the buffer file and records what it was shown. [`Pty`] runs a
//! program on a pseudo-terminal so prompts that need a real terminal can be driven.
//!
//! This file uses only the standard library, `libc` and `uuid` so it can be included both by the
//! integration tests (`mod editor;` in `tests/common/mod.rs`) and by thorctl's own unit tests
//! (`#[path]` include).

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// One thing the editor script does to the buffer file on one invocation
#[derive(Debug, Clone)]
pub enum Step {
    /// Replace every occurrence of one line-local text with another
    Replace(String, String),
    /// Replace the whole buffer with new content
    Write(String),
    /// Add text to the end of the buffer
    Append(String),
    /// Leave the buffer alone, as when a user saves without changing anything
    Noop,
    /// Exit with a failure code without touching the buffer
    Fail(i32),
}

impl Step {
    /// Replace `old` with `new` wherever it appears in the buffer (neither may hold a newline)
    ///
    /// # Arguments
    ///
    /// * `old` - The text to find
    /// * `new` - The text to put in its place
    pub fn replace(old: &str, new: &str) -> Self {
        Step::Replace(old.to_string(), new.to_string())
    }

    /// Replace the buffer with `content`
    ///
    /// # Arguments
    ///
    /// * `content` - The new buffer content
    pub fn write(content: &str) -> Self {
        Step::Write(content.to_string())
    }

    /// Add `text` after the buffer's last line
    ///
    /// # Arguments
    ///
    /// * `text` - The text to add
    pub fn append(text: &str) -> Self {
        Step::Append(text.to_string())
    }

    /// The shell commands that perform this step on the buffer file `$file`
    fn shell(&self) -> String {
        match self {
            // sed rewrites into a copy that is copied back so the buffer keeps its permissions
            Step::Replace(old, new) => format!(
                "sed 's/{}/{}/g' \"$file\" > \"$file.new\" && cat \"$file.new\" > \"$file\" && rm -f \"$file.new\"",
                sed_pattern(old),
                sed_replacement(new)
            ),
            Step::Write(content) => {
                format!("cat > \"$file\" <<'THORCTL_EDITOR_EOF'\n{content}\nTHORCTL_EDITOR_EOF")
            }
            Step::Append(text) => {
                format!("cat >> \"$file\" <<'THORCTL_EDITOR_EOF'\n{text}\nTHORCTL_EDITOR_EOF")
            }
            Step::Noop => ":".to_string(),
            Step::Fail(code) => format!("exit {code}"),
        }
    }
}

/// Escape text for the pattern side of a `sed` substitution that uses `/` as its delimiter
///
/// # Arguments
///
/// * `text` - The literal text to match
fn sed_pattern(text: &str) -> String {
    // quote the characters a basic regular expression treats specially, then single quotes
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' | '.' | '*' | '[' | ']' | '^' | '$' | '/' => {
                out.push('\\');
                out.push(c);
            }
            '\'' => out.push_str("'\\''"),
            other => out.push(other),
        }
    }
    out
}

/// Escape text for the replacement side of a `sed` substitution that uses `/` as its delimiter
///
/// # Arguments
///
/// * `text` - The literal replacement
fn sed_replacement(text: &str) -> String {
    // quote the characters a replacement treats specially, then single quotes
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' | '&' | '/' => {
                out.push('\\');
                out.push(c);
            }
            '\'' => out.push_str("'\\''"),
            other => out.push(other),
        }
    }
    out
}

/// A script to pass as `--editor` that edits the buffer file and records what it saw
///
/// The script does one [`Step`] per invocation, in order, and repeats the last step after that.
/// It copies the buffer it is given before acting, so tests can assert on what the user would
/// have seen (the header comment, read-only markers) with [`EditorScript::seen`].
pub struct EditorScript {
    /// The private directory holding the script, its call counter and the recorded buffers
    dir: PathBuf,
}

impl EditorScript {
    /// Build a script that performs `steps` on successive invocations
    ///
    /// # Arguments
    ///
    /// * `steps` - What to do on the first, second, ... invocation (the last repeats)
    pub fn sequence(steps: Vec<Step>) -> Self {
        use std::fmt::Write as _;
        // make a private directory for this script
        let dir =
            std::env::temp_dir().join(format!("thorctl-editor-script-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create the editor script directory");
        // count invocations in a file and keep each buffer that was shown
        let mut script = format!(
            "#!/bin/sh\ndir='{}'\nfile=\"$1\"\nn=$(cat \"$dir/calls\" 2>/dev/null || echo 0)\necho $((n+1)) > \"$dir/calls\"\ncp \"$file\" \"$dir/seen-$n\"\ncase \"$n\" in\n",
            dir.display()
        );
        // one case per step, with the last step as the default
        let count = steps.len();
        for (index, step) in steps.into_iter().enumerate() {
            let pattern = if index + 1 == count {
                "*".to_string()
            } else {
                index.to_string()
            };
            let _ = writeln!(script, "{pattern})\n{}\n;;", step.shell());
        }
        // an empty sequence leaves the buffer alone
        if count == 0 {
            script.push_str("*) : ;;\n");
        }
        script.push_str("esac\n");
        // write the script and make it executable
        let path = dir.join("editor.sh");
        std::fs::write(&path, script).expect("write the editor script");
        make_executable(&path);
        EditorScript { dir }
    }

    /// A script that replaces `old` with `new` in the buffer
    ///
    /// # Arguments
    ///
    /// * `old` - The text to find
    /// * `new` - The text to put in its place
    pub fn replace(old: &str, new: &str) -> Self {
        Self::sequence(vec![Step::replace(old, new)])
    }

    /// A script that replaces the whole buffer with `content`
    ///
    /// # Arguments
    ///
    /// * `content` - The new buffer content
    pub fn write(content: &str) -> Self {
        Self::sequence(vec![Step::write(content)])
    }

    /// A script that adds `text` to the end of the buffer
    ///
    /// # Arguments
    ///
    /// * `text` - The text to add
    pub fn append(text: &str) -> Self {
        Self::sequence(vec![Step::append(text)])
    }

    /// A script that saves the buffer unchanged
    pub fn noop() -> Self {
        Self::sequence(vec![Step::Noop])
    }

    /// A script that exits with a failure code
    ///
    /// # Arguments
    ///
    /// * `code` - The exit code
    pub fn fail(code: i32) -> Self {
        Self::sequence(vec![Step::Fail(code)])
    }

    /// The path of the script file
    pub fn script_path(&self) -> PathBuf {
        self.dir.join("editor.sh")
    }

    /// The value to pass as `--editor`
    ///
    /// The shell reads the script instead of the kernel executing it: running a file that was
    /// just written fails with "Text file busy" whenever another thread forks while the file is
    /// still open for writing, and the tests run in parallel.
    pub fn editor(&self) -> String {
        format!("sh '{}'", self.script_path().display())
    }

    /// How many times the editor was opened
    pub fn invocations(&self) -> usize {
        std::fs::read_to_string(self.dir.join("calls"))
            .ok()
            .and_then(|calls| calls.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The buffer the editor was shown on its nth invocation (0 is the first)
    ///
    /// # Arguments
    ///
    /// * `invocation` - Which invocation to read
    pub fn seen(&self, invocation: usize) -> Option<String> {
        std::fs::read_to_string(self.dir.join(format!("seen-{invocation}"))).ok()
    }
}

impl Drop for EditorScript {
    /// Remove the script directory
    fn drop(&mut self) {
        // cleanup is best effort
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Make a file executable on unix
///
/// # Arguments
///
/// * `path` - The file to make executable
fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .expect("read script metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("make the script executable");
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Find the recovery file an error names (`... (your edits were saved to '/tmp/...')`)
///
/// # Arguments
///
/// * `message` - The error text
pub fn recovery_path(message: &str) -> Option<PathBuf> {
    // take the text between the quotes that follow the marker
    let marker = "saved to '";
    let start = message.find(marker)? + marker.len();
    let end = message[start..].find('\'')? + start;
    Some(PathBuf::from(&message[start..end]))
}

// not every test binary that includes this file drives a terminal
#[allow(unused_imports)]
#[cfg(unix)]
pub use pty::Pty;

/// A program running on a pseudo-terminal, for driving prompts that need a real terminal
#[cfg(unix)]
mod pty {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::time::{Duration, Instant};

    /// A child process whose stdin, stdout and stderr are one pseudo-terminal
    pub struct Pty {
        /// The master side of the pseudo-terminal
        master: OwnedFd,
        /// The child process
        child: Child,
        /// Everything the child has written so far
        output: String,
        /// Where the last [`Pty::read_until`] match ended; the next one searches after it
        cursor: usize,
    }

    impl Pty {
        /// Start `program` with its standard streams on a new pseudo-terminal
        ///
        /// The child becomes a session leader with the terminal as its controlling terminal, so
        /// prompts that open `/dev/tty` work.
        ///
        /// # Arguments
        ///
        /// * `program` - The program to run
        /// * `args` - Its arguments
        /// * `envs` - Environment variables to set for it
        pub fn spawn(program: &Path, args: &[&str], envs: &[(&str, &str)]) -> io::Result<Self> {
            // open the pseudo-terminal pair
            let (mut master, mut slave) = (0, 0);
            // SAFETY: both pointers refer to live ints and the optional arguments are null
            let status = unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if status != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: openpty returned two fresh descriptors that nothing else owns
            let (master, slave) =
                unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
            // point all three standard streams at the slave
            let mut command = Command::new(program);
            // the default terminal type and color setting come first so a test can override them
            command
                .args(args)
                .env("TERM", "xterm")
                .env_remove("NO_COLOR")
                .envs(envs.iter().copied())
                .stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
            // SAFETY: only async-signal-safe calls (setsid, ioctl) run between fork and exec
            unsafe {
                command.pre_exec(|| {
                    // take the default action for the signals a test sends, whatever the test
                    // runner inherited (a background job starts with SIGINT ignored)
                    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                        libc::signal(signal, libc::SIG_DFL);
                    }
                    // start a new session and make stdin its controlling terminal
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn()?;
            Ok(Pty {
                master,
                child,
                output: String::new(),
                cursor: 0,
            })
        }

        /// Everything the child has written so far
        pub fn output(&self) -> &str {
            &self.output
        }

        /// Send a signal to the child process alone
        ///
        /// # Arguments
        ///
        /// * `signal` - The signal number (for example `libc::SIGTERM`)
        pub fn signal(&self, signal: i32) -> io::Result<()> {
            // the child's pid always fits a pid_t
            let pid = i32::try_from(self.child.id()).map_err(io::Error::other)?;
            // SAFETY: `kill` takes any pid and signal number
            if unsafe { libc::kill(pid, signal) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        /// Whether the terminal echoes what is typed (a password prompt turns this off)
        pub fn echo_enabled(&self) -> bool {
            // SAFETY: an all-zero termios is a valid value for `tcgetattr` to fill
            let mut settings: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: the pointer refers to a live termios and the descriptor is the open master
            let status = unsafe { libc::tcgetattr(self.master.as_raw_fd(), &raw mut settings) };
            status == 0 && settings.c_lflag & libc::ECHO != 0
        }

        /// Type text into the terminal
        ///
        /// # Arguments
        ///
        /// * `text` - The keys to send (`"\n"` for Enter)
        pub fn send(&mut self, text: &str) -> io::Result<()> {
            // write all of the text to the master side
            let mut written = 0;
            let bytes = text.as_bytes();
            while written < bytes.len() {
                // SAFETY: the pointer and length describe the live remainder of `bytes`
                let count = unsafe {
                    libc::write(
                        self.master.as_raw_fd(),
                        bytes[written..].as_ptr().cast(),
                        bytes.len() - written,
                    )
                };
                if count < 0 {
                    return Err(io::Error::last_os_error());
                }
                written += count.cast_unsigned();
            }
            Ok(())
        }

        /// Read what the child writes, once, waiting up to `wait` for the first bytes
        ///
        /// Returns `false` once the child closed the terminal.
        ///
        /// # Arguments
        ///
        /// * `wait` - How long to wait for output
        fn pump(&mut self, wait: Duration) -> bool {
            // wait for the master to become readable
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
            // SAFETY: the pointer refers to one live pollfd
            let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
            if ready <= 0 {
                return true;
            }
            // read what is there; a failed read means the child closed its side
            let mut buffer = [0u8; 4096];
            // SAFETY: the pointer and length describe the live buffer
            let count = unsafe {
                libc::read(
                    self.master.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if count <= 0 {
                return false;
            }
            self.output
                .push_str(&String::from_utf8_lossy(&buffer[..count.cast_unsigned()]));
            true
        }

        /// Read until the output contains `needle` after the previous match
        ///
        /// Successive calls match successive prompts, even when they print the same text.
        ///
        /// # Arguments
        ///
        /// * `needle` - The text to wait for
        /// * `timeout` - How long to wait before giving up with the output so far
        pub fn read_until(&mut self, needle: &str, timeout: Duration) -> Result<(), String> {
            // keep reading until the text shows up, the child closes, or time runs out
            let deadline = Instant::now() + timeout;
            loop {
                // a match moves the cursor past it
                if let Some(found) = self.output[self.cursor..].find(needle) {
                    self.cursor += found + needle.len();
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out waiting for {needle:?}; output so far:\n{}",
                        self.output
                    ));
                }
                if !self.pump(Duration::from_millis(100))
                    && !self.output[self.cursor..].contains(needle)
                {
                    return Err(format!(
                        "the program closed before printing {needle:?}; output:\n{}",
                        self.output
                    ));
                }
            }
        }

        /// Read to the end of the output and wait for the child to exit
        ///
        /// # Arguments
        ///
        /// * `timeout` - How long to wait before killing the child
        pub fn finish(&mut self, timeout: Duration) -> Result<ExitStatus, String> {
            // drain output until the child closes the terminal or time runs out
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if !self.pump(Duration::from_millis(100)) {
                    break;
                }
            }
            // collect the exit status, killing a child that is still running
            loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) if Instant::now() < deadline + Duration::from_secs(2) => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Ok(None) => {
                        let _ = self.child.kill();
                        return Err(format!(
                            "the program did not exit; output:\n{}",
                            self.output
                        ));
                    }
                    Err(err) => return Err(err.to_string()),
                }
            }
        }
    }

    impl Drop for Pty {
        /// Make sure the child does not outlive the test
        fn drop(&mut self) {
            // killing an exited child is harmless
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
