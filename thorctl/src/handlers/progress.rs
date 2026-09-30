//! Progress bar utilities
//!
//! Every progress bar in a thorctl run is drawn through one process-wide [`MultiProgress`],
//! so bars created by different helpers, worker pools, or nested steps share a single
//! redraw region instead of each assuming it owns the bottom lines of the terminal. All log
//! lines are printed through that same region (or with it suspended) so they land above the
//! live bars rather than being overwritten by the next redraw.
//!
//! Finished bars are moved out of the live region: their final line is printed as a normal
//! log line and the bar itself is cleared. That keeps the live region down to the bars that
//! are still running, so text printed after every bar has finished can't be erased by a later
//! redraw.

use indicatif::{
    MultiProgress, ProgressBar, ProgressDrawTarget, ProgressFinish, ProgressStyle, TermLike,
};
use owo_colors::OwoColorize;
use std::cell::Cell;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::{borrow::Cow, time::Duration};

/// The multi progress region every bar in this process is drawn in
static MULTI: LazyLock<MultiProgress> = LazyLock::new(MultiProgress::new);

thread_local! {
    /// Whether this thread is running inside [`suspend`], with the shared bars cleared and
    /// their lock held
    static SUSPENDED: Cell<bool> = const { Cell::new(false) };
}

/// Whether this thread is currently inside [`suspend`]
///
/// The shared region's lock is held for the whole suspension, so code running inside it must
/// print directly instead of going through the region (which would deadlock).
fn is_suspended() -> bool {
    // read this thread's flag
    SUSPENDED.with(Cell::get)
}

/// Whether progress bars are actually drawn
///
/// Bars draw on stderr, so they are hidden whenever stderr isn't a terminal. This is checked
/// directly rather than through the region so it is safe to call while suspended.
fn bars_visible() -> bool {
    // indicatif hides stderr targets that aren't terminals
    std::io::stderr().is_terminal()
}

/// Clears this thread's suspended flag when dropped, even if the suspended closure panics
struct SuspendGuard;

impl Drop for SuspendGuard {
    /// Mark this thread as no longer suspended
    fn drop(&mut self) {
        // clear this thread's flag
        SUSPENDED.with(|flag| flag.set(false));
    }
}

/// Hide every progress bar while `f` runs, then redraw them below whatever `f` printed
///
/// Use this around prompts, editors, and any direct `println!`/`eprintln!` made while a bar
/// may be live. Nested calls on the same thread just run `f`, since the bars are already
/// hidden. Progress bars must not be updated, finished, or dropped from inside `f`: the
/// region's lock is held until `f` returns.
///
/// # Arguments
///
/// * `f` - The closure to run while the bars are hidden
pub fn suspend<F: FnOnce() -> R, R>(f: F) -> R {
    // the bars are already hidden (and the lock held) when nested
    if is_suspended() {
        return f();
    }
    // clear the region, run the closure with this thread marked as suspended, then redraw
    MULTI.suspend(|| {
        SUSPENDED.with(|flag| flag.set(true));
        let _guard = SuspendGuard;
        f()
    })
}

/// Print a line on stderr without being overwritten by the progress bars
///
/// Visible bars get the line printed above them; otherwise (hidden bars, or inside
/// [`suspend`]) it is written straight to stderr.
///
/// # Arguments
///
/// * `line` - The line to print
fn print_stderr(line: &str) {
    // inside a suspension or without a terminal, nothing is drawn that could clobber the line
    if is_suspended() || !bars_visible() {
        eprintln!("{line}");
        return;
    }
    // print above the live bars, falling back to a plain line if the terminal write fails
    if MULTI.println(line).is_err() {
        eprintln!("{line}");
    }
}

/// Where a log line is written
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Sink {
    /// Above the live progress bars (stderr)
    Bars,
    /// Straight to stdout
    Stdout,
    /// Straight to stderr
    Stderr,
}

/// The kinds of log lines a [`Bar`] prints, which differ in where they fall back to
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Level {
    /// Informational lines shown only while the bar is visible
    Info,
    /// Informational lines that fall back to stdout when the bar is hidden
    InfoAnonymous,
    /// Warnings and errors, which always reach the user
    Alert,
}

/// Pick where a log line goes based on its level and the bar's state
///
/// # Arguments
///
/// * `level` - The kind of line being printed
/// * `has_bar` - Whether the bar exists (it doesn't in quiet mode)
/// * `visible` - Whether bars are drawn (stderr is a terminal)
fn sink(level: Level, has_bar: bool, visible: bool) -> Option<Sink> {
    match (level, has_bar, visible) {
        // a visible bar prints every line above the live bars
        (_, true, true) => Some(Sink::Bars),
        // warnings and errors always reach the user on stderr
        (Level::Alert, _, _) => Some(Sink::Stderr),
        // a hidden bar would swallow anonymous info, so it is printed on stdout
        (Level::InfoAnonymous, true, false) => Some(Sink::Stdout),
        // quiet mode, or plain info with a hidden bar, prints nothing
        _ => None,
    }
}

/// Write a log line to the sink chosen for it
///
/// # Arguments
///
/// * `sink` - Where to write the line, or `None` to drop it
/// * `line` - The line to write
fn emit(sink: Option<Sink>, line: &str) {
    match sink {
        // print above the bars without being overwritten
        Some(Sink::Bars) => print_stderr(line),
        // hidden bars draw nothing, so plain prints are safe
        Some(Sink::Stdout) => println!("{line}"),
        Some(Sink::Stderr) => eprintln!("{line}"),
        None => (),
    }
}

/// A terminal stand-in that records what a progress bar draws, used to render a bar's final
/// line so it can be printed as plain text
#[derive(Debug, Clone, Default)]
struct Capture {
    /// The text written so far
    buf: Arc<Mutex<String>>,
}

impl Capture {
    /// Take the captured text as a single line, without carriage returns or padding
    fn take(&self) -> String {
        // take the text, tolerating a poisoned lock since the buffer is only ever appended to
        let text = std::mem::take(&mut *self.buf.lock().unwrap_or_else(PoisonError::into_inner));
        // drop the carriage returns and the padding indicatif writes after the last line
        text.replace('\r', "").trim_end().to_string()
    }

    /// Append text to the capture buffer
    ///
    /// # Arguments
    ///
    /// * `text` - The text to append
    fn push(&self, text: &str) {
        // append, tolerating a poisoned lock since the buffer is only ever appended to
        self.buf
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_str(text);
    }
}

impl TermLike for Capture {
    /// Render at the real terminal's width so the line matches what the bar showed
    fn width(&self) -> u16 {
        // fall back to 80 columns when the size is unknown or reported as zero (some ptys)
        crossterm::terminal::size()
            .ok()
            .map(|(cols, _)| cols)
            .filter(|cols| *cols > 0)
            .unwrap_or(80)
    }

    /// Never truncate the rendered lines for height
    fn height(&self) -> u16 {
        u16::MAX
    }

    /// Cursor movement has no meaning for a capture
    ///
    /// # Arguments
    ///
    /// * `_n` - The number of lines to move
    fn move_cursor_up(&self, _n: usize) -> std::io::Result<()> {
        Ok(())
    }

    /// Cursor movement has no meaning for a capture
    ///
    /// # Arguments
    ///
    /// * `_n` - The number of lines to move
    fn move_cursor_down(&self, _n: usize) -> std::io::Result<()> {
        Ok(())
    }

    /// Cursor movement has no meaning for a capture
    ///
    /// # Arguments
    ///
    /// * `_n` - The number of columns to move
    fn move_cursor_right(&self, _n: usize) -> std::io::Result<()> {
        Ok(())
    }

    /// Cursor movement has no meaning for a capture
    ///
    /// # Arguments
    ///
    /// * `_n` - The number of columns to move
    fn move_cursor_left(&self, _n: usize) -> std::io::Result<()> {
        Ok(())
    }

    /// Record a line of text
    ///
    /// # Arguments
    ///
    /// * `s` - The line to record
    fn write_line(&self, s: &str) -> std::io::Result<()> {
        // record the line followed by its newline
        self.push(s);
        self.push("\n");
        Ok(())
    }

    /// Record a fragment of text
    ///
    /// # Arguments
    ///
    /// * `s` - The text to record
    fn write_str(&self, s: &str) -> std::io::Result<()> {
        self.push(s);
        Ok(())
    }

    /// Clearing has no meaning for a capture
    fn clear_line(&self) -> std::io::Result<()> {
        Ok(())
    }

    /// Nothing is buffered outside the capture
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Render the line a progress bar would show once finished
///
/// Draws a throwaway copy of the bar (same style, message, position, length and elapsed
/// time) into a [`Capture`] so the result matches indicatif's own rendering exactly. Like
/// indicatif's `finish`, a bounded bar is drawn as complete.
///
/// # Arguments
///
/// * `pb` - The progress bar to render
fn render_final(pb: &ProgressBar) -> String {
    // draw a copy of the bar into a capture instead of the terminal
    let capture = Capture::default();
    let target = ProgressDrawTarget::term_like(Box::new(capture.clone()));
    let ghost = ProgressBar::with_draw_target(pb.length(), target)
        .with_style(pb.style())
        .with_message(pb.message())
        .with_position(pb.position())
        .with_elapsed(pb.elapsed());
    // finishing forces exactly one draw of the final state
    ghost.finish();
    capture.take()
}

/// Finish a bar, keeping its final line on screen as plain text
///
/// A visible bar is cleared from the live region and its final line printed above the
/// remaining bars; a hidden bar is simply marked finished.
///
/// # Arguments
///
/// * `pb` - The progress bar to finish
fn leave(pb: &ProgressBar) {
    // hidden bars draw nothing, so there is no line to keep
    if !bars_visible() || is_suspended() {
        pb.finish();
        return;
    }
    // render the final line before the bar is cleared
    let line = render_final(pb);
    // remove the bar from the live region
    pb.finish_and_clear();
    // keep its last state as a permanent line above the remaining bars
    if !line.is_empty() {
        print_stderr(&line);
    }
}

/// Create a progress bar drawn in the shared region
///
/// The bar is added to the region before it is configured, so a steady tick can never draw it
/// on its own. If it is dropped without being finished it clears itself.
///
/// # Arguments
///
/// * `pb` - The (not yet drawn) progress bar to add
fn register(pb: ProgressBar) -> ProgressBar {
    // clear on drop and draw only through the shared region
    MULTI.add(pb.with_finish(ProgressFinish::AndClear))
}

/// A progress bar shared by every clone of a [`Bar`]
///
/// When the last clone is dropped an unfinished bar is finished and its final line kept, so a
/// bar abandoned by an early return leaves its last state on screen without becoming a
/// stale, redrawn line in the live region.
struct LiveBar {
    /// The progress bar drawn in the shared region
    pb: ProgressBar,
    /// Whether the bar's final line has already been printed since it was last (re)started
    left: AtomicBool,
}

impl LiveBar {
    /// Wrap a progress bar that has been added to the shared region
    ///
    /// # Arguments
    ///
    /// * `pb` - The registered progress bar
    fn new(pb: ProgressBar) -> Arc<Self> {
        Arc::new(LiveBar {
            pb,
            left: AtomicBool::new(false),
        })
    }
}

impl Drop for LiveBar {
    /// Finish the bar if nothing else did
    fn drop(&mut self) {
        // touching the bar while suspended would deadlock on the region's lock
        if !is_suspended() && !self.pb.is_finished() {
            leave(&self.pb);
        }
    }
}

/// The different types of progress bars
pub enum BarKind {
    /// A bar with only a simple timer
    Timer,
    /// An unbounded bar with a spinner
    Unbound,
    /// A bounded bar
    Bound(u64),
    /// A bounded bar that also displays the rate of progress per second
    BoundRate(u64),
    /// An unbounded IO bar
    UnboundIO,
    /// An IO bar
    IO(u64),
}

impl BarKind {
    /// The speed at which to spin the spinner
    const SPINNER_INTERVAL_MILLIS: u64 = 100;

    /// Configure this bar to the right type
    ///
    /// # Arguments
    ///
    /// * `name` - The name for this bar
    pub fn setup(self, name: &str, bar: &ProgressBar) {
        // configure this bar to the right kind
        match self {
            BarKind::Timer => {
                // build our style string
                let style = format!("[{{elapsed_precise}}] {{spinner}} {name} {{msg}}");
                // set the style for our bar
                bar.set_style(ProgressStyle::with_template(&style).unwrap());
                bar.enable_steady_tick(Duration::from_millis(Self::SPINNER_INTERVAL_MILLIS));
            }
            BarKind::Unbound => {
                // build our style string
                let style = format!("[{{elapsed_precise}}] {name} {{pos}} {{msg}}");
                // set the style for our bar
                bar.set_style(ProgressStyle::with_template(&style).unwrap());
            }
            BarKind::Bound(bound) => {
                // build our style string
                let style = format!(
                    "[{{elapsed_precise}}] {name} {{msg}} {{bar:40.cyan/blue}} {{pos:>7}}/{{len:7}} {{eta}} remaining"
                );
                // set the style for our bar
                bar.set_style(ProgressStyle::with_template(&style).unwrap());
                // set this bars length
                bar.set_length(bound);
                // start this bars progress at 0
                bar.set_position(0);
            }
            BarKind::BoundRate(bound) => {
                // build our style string, including the per-second rate of progress
                let style = format!(
                    "[{{elapsed_precise}}] {name} {{msg}} {{bar:40.cyan/blue}} {{pos:>7}}/{{len:7}} ({{per_sec}}) {{eta}} remaining"
                );
                // set the style for our bar
                bar.set_style(ProgressStyle::with_template(&style).unwrap());
                // set this bars length
                bar.set_length(bound);
                // start this bars progress at 0
                bar.set_position(0);
            }
            BarKind::UnboundIO => {
                // build our style string
                let style = format!(
                    "[{{elapsed_precise}}] {name} {{msg}} {{bytes}} {{binary_bytes_per_sec}}"
                );
                // set the style for our bar
                bar.set_style(ProgressStyle::with_template(&style).unwrap());
                // start this bars progress at 0
                bar.set_position(0);
            }
            BarKind::IO(bound) => {
                // build our style string
                let style = format!(
                    "[{{elapsed_precise}}] {name} {{msg}} {{bytes}}/{{total_bytes}} {{binary_bytes_per_sec}}"
                );
                // set this bars length
                bar.set_length(bound);
                // start this bars progress at 0
                bar.set_position(0);
                // set the style for our bar
                bar.set_style(ProgressStyle::with_template(&style).unwrap());
            }
        }
    }
}

/// The controller for multiple progress bars in Thorctl
#[derive(Clone)]
pub struct MultiBar {
    /// The shared progress region, or `None` in quiet mode
    multi: Option<MultiProgress>,
}

impl MultiBar {
    /// Create a new multi progress bar
    ///
    /// # Arguments
    ///
    /// * `quiet` - Whether this progress bar should be visible
    pub fn new(quiet: bool) -> Self {
        // quiet mode creates no bars; otherwise bars join the shared region
        let multi = if quiet { None } else { Some(MULTI.clone()) };
        MultiBar { multi }
    }

    /// Add child progress bar
    ///
    /// # Arguments
    ///
    /// * `name` - The name identifying this bar
    /// * `kind` - The kind of bar to add
    pub fn add(&self, name: &str, kind: BarKind) -> Bar {
        // if quiet is set then don't create a bar
        let bar = self.multi.as_ref().map(|_| {
            // add a new bar to the shared region before configuring it
            let pb = register(ProgressBar::new_spinner());
            // configure our new bar
            kind.setup(name, &pb);
            LiveBar::new(pb)
        });
        // build our child progress bar
        Bar {
            name: name.to_owned(),
            bar,
        }
    }

    /// Print an Error message
    ///
    /// # Arguments
    ///
    /// * `msg` - The error message to print
    pub fn error(&self, msg: &str) {
        match &self.multi {
            // print above the bars when they're drawn, otherwise straight to stderr
            Some(_) => print_stderr(msg),
            // quiet mode draws no bars, so a plain line is safe
            None => eprintln!("{msg}"),
        }
    }
}

/// A single progress bar in Thorctl
#[derive(Clone)]
pub struct Bar {
    /// The name of this progress bar
    name: String,
    /// The progress bar to use when showing progress, or `None` in quiet mode
    bar: Option<Arc<LiveBar>>,
}

impl Bar {
    /// Create a new progress bar
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the progress bar
    /// * `msg` - The message to display
    /// * `kind` - The kind of progress bar to make
    pub fn new<T, M>(name: T, msg: M, kind: BarKind) -> Self
    where
        T: Into<String>,
        M: Into<Cow<'static, str>>,
    {
        // add a bar to the shared region, then configure it
        let bar = Self {
            name: name.into(),
            bar: Some(LiveBar::new(register(ProgressBar::new(0)))),
        };
        bar.refresh(msg, kind);
        bar
    }

    /// Create a new progress bar, or an inert one in quiet mode
    ///
    /// A quiet bar draws nothing and suppresses info lines, while warnings and
    /// errors are still printed.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the progress bar
    /// * `msg` - The message to display
    /// * `kind` - The kind of progress bar to make
    /// * `quiet` - Whether to create an inert quiet bar instead
    pub fn new_or_quiet<T, M>(name: T, msg: M, kind: BarKind, quiet: bool) -> Self
    where
        T: Into<String>,
        M: Into<Cow<'static, str>>,
    {
        // quiet mode gets a bar with nothing to draw
        if quiet {
            return Self {
                name: name.into(),
                bar: None,
            };
        }
        // otherwise build a normal visible bar
        Self::new(name, msg, kind)
    }

    /// Create a simple progress timer with no bound or length
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the progress bar
    /// * `msg` - The message to display
    pub fn new_unbounded<T, M>(name: T, msg: M) -> Self
    where
        T: Into<String>,
        M: Into<Cow<'static, str>>,
    {
        // add a spinner to the shared region, then configure it
        let bar = Self {
            name: name.into(),
            bar: Some(LiveBar::new(register(ProgressBar::new_spinner()))),
        };
        bar.refresh(msg, BarKind::Unbound);
        bar
    }

    /// The underlying progress bar, or `None` in quiet mode
    fn pb(&self) -> Option<&ProgressBar> {
        // reach through the shared handle to the bar itself
        self.bar.as_deref().map(|live| &live.pb)
    }

    /// A handle to the underlying progress bar, for APIs that drive one directly
    ///
    /// The handle draws in the shared region like this bar does.
    pub fn progress_bar(&self) -> Option<ProgressBar> {
        // clone the handle so it can be driven independently of this wrapper
        self.pb().cloned()
    }

    /// Whether this bar is actually drawn (it exists and stderr is a terminal)
    pub fn is_visible(&self) -> bool {
        // quiet bars never draw, and no bar draws without a terminal on stderr
        self.bar.is_some() && bars_visible()
    }

    /// Change this bars style
    ///
    /// # Arguments
    ///
    /// * `style` - The style to set
    pub fn set_style(&self, style: ProgressStyle) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            // update our bars style
            bar.set_style(style);
        }
    }

    /// Set a steady tick rate for our bar
    ///
    /// # Arguments
    ///
    /// * `tick` - The tick duration to set
    pub fn enable_steady_tick(&self, tick: Duration) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            bar.enable_steady_tick(tick);
        }
    }

    /// Rename this bar
    ///
    /// # Arguments
    ///
    /// * `name` - The name to use for this bar going forward
    pub fn rename(&mut self, name: String) {
        self.name = name;
    }

    /// Set a new message for this bar
    ///
    /// # Arguments
    ///
    /// * `msg` - The message to set
    pub fn set_message<M: Into<Cow<'static, str>>>(&self, msg: M) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            // set our new message
            bar.set_message(msg);
        }
    }

    /// Set the length for this bar
    ///
    /// # Arguments
    ///
    /// * `len` - The length to set
    #[allow(dead_code)]
    pub fn set_length(&self, len: u64) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            bar.set_length(len);
        }
    }

    /// Increment our total length
    ///
    /// # Arguments
    ///
    /// * `delta` - The delta to apply
    pub fn inc_length(&self, delta: u64) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            bar.inc_length(delta);
        }
    }

    /// Increment our progress
    ///
    /// # Arguments
    ///
    /// * `delta` - The delta to apply
    pub fn inc(&self, delta: u64) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            bar.inc(delta);
        }
    }

    /// Set the position of our progress bar
    ///
    /// # Arguments
    ///
    /// * `position` - The new position to set
    pub fn set_position(&self, position: u64) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            bar.set_position(position);
        }
    }

    /// Format a log line as `<level>: <name> - <msg>`, or `<level>: <msg>` when
    /// this bar has no name so unnamed bars don't print a dangling separator
    ///
    /// # Arguments
    ///
    /// * `level` - The (colorized) level prefix such as `Info` or `Error`
    /// * `msg` - The message to print
    fn line<L: std::fmt::Display>(&self, level: L, msg: &str) -> String {
        format_line(level, &self.name, msg)
    }

    /// Print a line through the sink its level and this bar's state call for
    ///
    /// # Arguments
    ///
    /// * `level` - The kind of line being printed
    /// * `line` - The fully formatted line
    fn log(&self, level: Level, line: &str) {
        // route the line by level, whether this bar exists, and whether bars are drawn
        emit(sink(level, self.bar.is_some(), bars_visible()), line);
    }

    /// Print an info message
    ///
    /// Info lines are only shown while the bar is visible.
    ///
    /// # Arguments
    ///
    /// * `msg` - The info message to print
    pub fn info<T: AsRef<str>>(&self, msg: T) {
        // format with the bar's name and print only if the bar is visible
        self.log(Level::Info, &self.line("Info".bright_blue(), msg.as_ref()));
    }

    /// Print an info message without the bar's name included
    ///
    /// When the bar is hidden (output is not a tty) the line is printed to stdout
    /// instead of being swallowed; quiet mode (no bar at all) still suppresses it.
    ///
    /// # Arguments
    ///
    /// * `msg` - The info message to print
    pub fn info_anonymous<T: AsRef<str>>(&self, msg: T) {
        // format without the bar's name, falling back to stdout when the bar is hidden
        let line = format!("{}: {}", "Info".bright_blue(), msg.as_ref());
        self.log(Level::InfoAnonymous, &line);
    }

    /// Print a warning message
    ///
    /// Warnings must reach the user even in quiet mode or when output is not a
    /// tty, so this falls back to stderr whenever the bar can't display it.
    ///
    /// # Arguments
    ///
    /// * `msg` - The warning message to print
    pub fn warning<T: AsRef<str>>(&self, msg: T) {
        // format with the bar's name; warnings always reach the user
        let line = self.line("Warning".bright_yellow(), msg.as_ref());
        self.log(Level::Alert, &line);
    }

    /// Print an error message
    ///
    /// Errors must reach the user even when output is not a tty, so this falls
    /// back to stderr whenever the bar can't display it.
    ///
    /// # Arguments
    ///
    /// * `msg` - The error message to print
    pub fn error<T: AsRef<str>>(&self, msg: T) {
        // format with the bar's name; errors always reach the user
        let line = self.line("Error".bright_red(), msg.as_ref());
        self.log(Level::Alert, &line);
    }

    /// Print an error message without the bar's name included
    ///
    /// # Arguments
    ///
    /// * `msg` - The error message to print
    pub fn error_anonymous<T: AsRef<str>>(&self, msg: T) {
        // format without the bar's name; errors always reach the user
        let line = format!("{}: {}", "Error".bright_red(), msg.as_ref());
        self.log(Level::Alert, &line);
    }

    /// Set a new message for this bar.
    ///
    /// This does not change the bars name. A finished bar is restarted and drawn again.
    ///
    /// # Arguments
    ///
    /// * `msg` - The message to set
    /// * `kind` - The kind of bar to switch to
    pub fn refresh<M: Into<Cow<'static, str>>>(&self, msg: M, kind: BarKind) {
        // check if we are in quiet mode or not
        if let Some(live) = &self.bar {
            let bar = &live.pb;
            // a restarted bar gets a new final line when it finishes again
            live.left.store(false, Ordering::Relaxed);
            // reset any progress in this bar
            bar.reset();
            // resetup our bar
            kind.setup(&self.name, bar);
            // set our new message
            bar.set_message(msg);
        }
    }

    /// Finish the bar, leaving its final line displayed above any bars still running
    ///
    /// Finishing a bar again without refreshing it in between prints nothing more.
    pub fn finish(&self) {
        // check if we are in quiet mode, and print the final line only once
        if let Some(live) = &self.bar
            && !live.left.swap(true, Ordering::Relaxed)
        {
            leave(&live.pb);
        }
    }

    /// Finish this bar with an updated message
    ///
    /// # Arguments
    ///
    /// * `msg` - The final message to show
    pub fn finish_with_message<M: Into<Cow<'static, str>>>(&self, msg: M) {
        // check if we are in quiet mode or not
        if let Some(live) = &self.bar {
            // set the final message, then keep the finished line
            live.pb.set_message(msg);
            live.left.store(true, Ordering::Relaxed);
            leave(&live.pb);
        }
    }

    /// Finish this bar and clear it
    pub fn finish_and_clear(&self) {
        // check if we are in quiet mode or not
        if let Some(bar) = self.pb() {
            bar.finish_and_clear();
        }
    }

    /// Clear this bar if it hasn't been finished, leaving finished bars untouched
    ///
    /// Used for bars that may never have been given work (such as idle worker bars) so
    /// they don't leave a stale line behind.
    pub fn clear_if_unfinished(&self) {
        // only an unfinished bar is still in the live region
        if let Some(bar) = self.pb()
            && !bar.is_finished()
        {
            bar.finish_and_clear();
        }
    }

    /// Suspend every progress bar while the function `f` is executing
    ///
    /// Every bar shares one region, so this hides all of them, not just this one; it stays a
    /// method so call sites read as suspending the bar they log through. See [`suspend`];
    /// progress bars must not be updated from inside `f`.
    ///
    /// # Arguments
    ///
    /// * `f` - The closure to run while the bars are hidden
    #[allow(clippy::unused_self)]
    pub fn suspend<F: FnOnce() -> R, R>(&self, f: F) -> R {
        suspend(f)
    }

    /// Suspend every progress bar while the future `f` is executing.
    /// Uses [`tokio::task::block_in_place`] to bridge indicatif's sync
    /// `suspend` with an async future.
    ///
    /// # Arguments
    ///
    /// * `f` - The future to execute while the progress bars are suspended
    pub async fn suspend_async<F, R>(&self, f: F) -> R
    where
        F: std::future::Future<Output = R>,
    {
        match &self.bar {
            // block this worker thread on the future with the bars hidden
            Some(_) => tokio::task::block_in_place(|| {
                suspend(|| tokio::runtime::Handle::current().block_on(f))
            }),
            // quiet mode has no bar of its own to hide
            None => f.await,
        }
    }
}

/// Format a log line as `<level>: <name> - <msg>`, dropping the name and its
/// separator when `name` is empty
///
/// # Arguments
///
/// * `level` - The (colorized) level prefix such as `Info` or `Error`
/// * `name` - The name of the bar the line is logged through
/// * `msg` - The message to print
fn format_line<L: std::fmt::Display>(level: L, name: &str, msg: &str) -> String {
    // unnamed bars print just the level and message
    if name.is_empty() {
        format!("{level}: {msg}")
    } else {
        format!("{level}: {name} - {msg}")
    }
}

/// Print a warning to stderr with the same colorized prefix [`Bar::warning`] uses
///
/// For contexts that have no [`Bar`] (a synchronous routine, or a direct print) but should still
/// match the canonical warning style. Goes to stderr so it isn't swallowed in quiet/non-tty runs,
/// above any live progress bars so a redraw can't overwrite it.
///
/// # Arguments
///
/// * `msg` - The warning message to print
pub fn warn<T: AsRef<str>>(msg: T) {
    print_stderr(&format!("{}: {}", "Warning".bright_yellow(), msg.as_ref()));
}

/// Print a note to stderr with a colorized prefix, for advisory one-off messages
///
/// The no-[`Bar`] companion to [`warn`], for informational notices that aren't warnings or errors.
///
/// # Arguments
///
/// * `msg` - The note message to print
pub fn note<T: AsRef<str>>(msg: T) {
    print_stderr(&format!("{}: {}", "Note".bright_yellow(), msg.as_ref()));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A named bar prefixes its lines with the bar's name
    #[test]
    fn format_line_includes_name() {
        assert_eq!(
            format_line("Warning", "images export", "hi"),
            "Warning: images export - hi"
        );
    }

    /// An unnamed bar omits the name and its separator
    #[test]
    fn format_line_omits_empty_name() {
        assert_eq!(format_line("Warning", "", "hi"), "Warning: hi");
    }

    /// Visible bars print every level above themselves
    #[test]
    fn sink_visible_bar_prints_above_bars() {
        for level in [Level::Info, Level::InfoAnonymous, Level::Alert] {
            assert_eq!(sink(level, true, true), Some(Sink::Bars));
        }
    }

    /// Hidden bars keep warnings/errors on stderr and anonymous info on stdout, and drop info
    #[test]
    fn sink_hidden_bar_falls_back_to_plain_streams() {
        assert_eq!(sink(Level::Alert, true, false), Some(Sink::Stderr));
        assert_eq!(sink(Level::InfoAnonymous, true, false), Some(Sink::Stdout));
        assert_eq!(sink(Level::Info, true, false), None);
    }

    /// Quiet mode (no bar) only lets warnings and errors through, on stderr
    #[test]
    fn sink_quiet_only_prints_alerts() {
        for visible in [true, false] {
            assert_eq!(sink(Level::Alert, false, visible), Some(Sink::Stderr));
            assert_eq!(sink(Level::InfoAnonymous, false, visible), None);
            assert_eq!(sink(Level::Info, false, visible), None);
        }
    }

    /// Nested suspends run their closures without deadlocking on the shared region, and the
    /// suspended flag is cleared afterwards
    #[test]
    fn suspend_is_reentrant() {
        let value = suspend(|| {
            // printing and nesting while suspended must not touch the region's lock
            assert!(is_suspended());
            warn("nested warning");
            suspend(|| 7)
        });
        assert_eq!(value, 7);
        assert!(!is_suspended());
    }

    /// A finished bar renders the same line indicatif would draw for it
    #[test]
    fn render_final_matches_bar_template() {
        let pb = ProgressBar::hidden();
        BarKind::Unbound.setup("images import", &pb);
        pb.set_position(3);
        pb.set_message("done");
        assert_eq!(render_final(&pb), "[00:00:00] images import 3 done");
    }

    /// A bounded bar's final line shows it completed, as indicatif's own `finish` does
    #[test]
    fn render_final_keeps_bounded_progress() {
        let pb = ProgressBar::hidden();
        BarKind::Bound(4).setup("Exporting Images", &pb);
        pb.set_position(2);
        let line = render_final(&pb);
        assert!(line.starts_with("[00:00:00] Exporting Images"), "{line}");
        assert!(line.contains("4/4"), "{line}");
    }

    /// Concurrent bars can log, suspend, finish, and drop without deadlocking on the shared
    /// region, whether or not stderr is a terminal
    #[test]
    fn concurrent_bars_share_the_region() {
        let parent = Bar::new("parent", "working", BarKind::Timer);
        std::thread::scope(|scope| {
            for i in 0..4 {
                let parent = parent.clone();
                scope.spawn(move || {
                    // each worker drives its own bar while logging through the parent
                    let child = Bar::new(format!("child {i}"), "step", BarKind::Bound(3));
                    for _ in 0..3 {
                        child.inc(1);
                        parent.info(format!("child {i} stepped"));
                        child.suspend(|| warn(format!("child {i} suspended")));
                    }
                    child.finish();
                });
            }
        });
        parent.finish_with_message("done");
        parent.finish();
    }

    /// Quiet bars accept every call without drawing or printing
    #[test]
    fn quiet_bar_is_inert() {
        let bar = MultiBar::new(true).add("quiet", BarKind::Timer);
        assert!(!bar.is_visible());
        bar.info("hidden");
        bar.info_anonymous("hidden");
        bar.inc(1);
        bar.finish();
        bar.clear_if_unfinished();
        assert_eq!(bar.suspend(|| 1), 1);
    }

    /// A bar created in quiet mode is inert, like a quiet multi bar child
    #[test]
    fn new_or_quiet_quiet_bar_is_inert() {
        let bar = Bar::new_or_quiet("quiet", "working", BarKind::Timer, true);
        assert!(bar.progress_bar().is_none());
        assert!(!bar.is_visible());
        bar.info_anonymous("hidden");
        bar.finish();
    }
}
