//! Kill helpers: run a child process and SIGKILL it once it logs a chosen event
//!
//! Interrupted backups and restores are tested by running the thoradm binary as a child and
//! killing it at a known point, so the test process itself is never affected.

use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};

/// What happened to a child run by [`run_until`]
#[derive(Debug)]
pub struct KillOutcome {
    /// Whether the child was killed after logging the event
    pub killed: bool,
    /// How the child exited
    pub status: ExitStatus,
    /// Every line the child wrote to stderr before it exited
    pub stderr: Vec<String>,
    /// How long after the event the child was gone
    pub after_event: Option<Duration>,
}

/// Sends SIGKILL to every process in a child's process group
///
/// # Arguments
///
/// * `pid` - The child's process id, which is also its process group id
fn kill_group(pid: u32) -> std::io::Result<()> {
    // a negative pid addresses the whole process group
    let group = -i32::try_from(pid).map_err(std::io::Error::other)?;
    // SAFETY: kill only sends a signal; it touches no memory of this process
    let result = unsafe { libc::kill(group, libc::SIGKILL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Runs a command and sends it SIGKILL as soon as a stderr line contains a pattern
///
/// The child runs in its own process group and the whole group is killed, so processes it
/// started can't keep its stderr open. The child runs to completion when the pattern never
/// appears.
///
/// # Arguments
///
/// * `command` - The command to run; its stderr is captured
/// * `pattern` - The text whose first appearance on stderr triggers the kill
pub async fn run_until(mut command: Command, pattern: &str) -> std::io::Result<KillOutcome> {
    // put the child in a process group of its own so the kill reaches everything it starts
    command.process_group(0);
    // spawn the child with stderr piped so its progress can be watched
    let mut command = tokio::process::Command::from(command);
    command
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stderr = child.stderr.take().expect("stderr was piped");
    let mut lines = BufReader::new(stderr).lines();
    let mut captured = Vec::new();
    let mut killed_at = None;
    // read stderr line by line until the child closes it
    while let Some(line) = lines.next_line().await? {
        // kill the child the first time the pattern shows up
        if killed_at.is_none() && line.contains(pattern) {
            kill_group(child.id().expect("the child is still running"))?;
            killed_at = Some(Instant::now());
        }
        captured.push(line);
    }
    // wait for the child to exit
    let status = child.wait().await?;
    Ok(KillOutcome {
        killed: killed_at.is_some(),
        status,
        stderr: captured,
        after_event: killed_at.map(|at| at.elapsed()),
    })
}

/// Runs a command and sends it SIGKILL as soon as a check passes, polling the check while the
/// child runs
///
/// The child runs in its own process group and the whole group is killed. The child runs to
/// completion when the check never passes.
///
/// # Arguments
///
/// * `command` - The command to run; its stderr is captured
/// * `found` - The check, usually of files the child writes, whose first pass triggers the kill
pub async fn run_until_found<F: Fn() -> bool>(
    mut command: Command,
    found: F,
) -> std::io::Result<KillOutcome> {
    // put the child in a process group of its own so the kill reaches everything it starts
    command.process_group(0);
    // spawn the child with stderr piped and read on its own task, so a full pipe never blocks it
    let mut command = tokio::process::Command::from(command);
    command
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stderr = child.stderr.take().expect("stderr was piped");
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut captured = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            captured.push(line);
        }
        captured
    });
    // poll the check until it passes or the child exits on its own
    let mut killed_at = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if found() {
            kill_group(child.id().expect("the child is still running"))?;
            killed_at = Some(Instant::now());
            break child.wait().await?;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    // collect what the child wrote before it exited
    let captured = reader.await.map_err(std::io::Error::other)?;
    Ok(KillOutcome {
        killed: killed_at.is_some(),
        status,
        stderr: captured,
        after_event: killed_at.map(|at| at.elapsed()),
    })
}
