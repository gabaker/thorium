//! Tests for the editor script helper that the interactive tests are built on

use std::path::PathBuf;
use std::process::Command;

use super::editor::{EditorScript, Step, recovery_path};

/// A buffer file in its own directory, removed when dropped
struct Buffer(PathBuf);

impl Buffer {
    /// Create a buffer file holding `content`
    ///
    /// # Arguments
    ///
    /// * `content` - The initial content
    fn new(content: &str) -> Self {
        // use a fresh directory so tests never collide
        let dir =
            std::env::temp_dir().join(format!("thorctl-buffer-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("buffer.yml");
        std::fs::write(&path, content).unwrap();
        Buffer(path)
    }

    /// Open the buffer in the script once, like the editor loop does
    ///
    /// # Arguments
    ///
    /// * `script` - The editor script
    fn open(&self, script: &EditorScript) -> i32 {
        // run the script with the buffer path as its argument
        Command::new("sh")
            .arg(script.script_path())
            .arg(&self.0)
            .status()
            .unwrap()
            .code()
            .unwrap()
    }

    /// The buffer's current content
    fn text(&self) -> String {
        std::fs::read_to_string(&self.0).unwrap()
    }
}

impl Drop for Buffer {
    /// Remove the buffer's directory
    fn drop(&mut self) {
        // cleanup is best effort
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

/// Replacing treats regular expression characters, quotes and slashes literally
#[test]
fn replace_is_literal() {
    // replace text full of characters that sed treats specially
    let buffer = Buffer::new("a.b*c [x] it's & a/b\nsecond a.b*c\n");
    let script = EditorScript::replace("a.b*c", "<&/'\\>");
    assert_eq!(buffer.open(&script), 0);
    // only the literal text was replaced, on every line
    assert_eq!(buffer.text(), "<&/'\\> [x] it's & a/b\nsecond <&/'\\>\n");
    // text that only matches as a pattern is left alone
    let other = Buffer::new("axb\n");
    assert_eq!(other.open(&EditorScript::replace("a.b", "z")), 0);
    assert_eq!(other.text(), "axb\n");
}

/// Write replaces the buffer and append adds to it
#[test]
fn write_and_append() {
    // write over the buffer
    let buffer = Buffer::new("old\n");
    assert_eq!(buffer.open(&EditorScript::write("new: 1\nlines: 2")), 0);
    assert_eq!(buffer.text(), "new: 1\nlines: 2\n");
    // append after it
    assert_eq!(buffer.open(&EditorScript::append("# tail")), 0);
    assert_eq!(buffer.text(), "new: 1\nlines: 2\n# tail\n");
}

/// A sequence does one step per invocation and repeats the last
#[test]
fn sequence_steps_and_repeats() {
    // three invocations of a two-step sequence
    let script = EditorScript::sequence(vec![Step::write("first"), Step::append("again")]);
    let buffer = Buffer::new("start\n");
    assert_eq!(buffer.open(&script), 0);
    assert_eq!(buffer.text(), "first\n");
    assert_eq!(buffer.open(&script), 0);
    assert_eq!(buffer.text(), "first\nagain\n");
    assert_eq!(buffer.open(&script), 0);
    assert_eq!(buffer.text(), "first\nagain\nagain\n");
    // every buffer shown was recorded
    assert_eq!(script.invocations(), 3);
    assert_eq!(script.seen(0).as_deref(), Some("start\n"));
    assert_eq!(script.seen(2).as_deref(), Some("first\nagain\n"));
    assert_eq!(script.seen(3), None);
}

/// A noop leaves the buffer alone and a fail exits with its code
#[test]
fn noop_and_fail() {
    // a noop changes nothing
    let buffer = Buffer::new("same\n");
    assert_eq!(buffer.open(&EditorScript::noop()), 0);
    assert_eq!(buffer.text(), "same\n");
    // a failure exits with its code and changes nothing
    assert_eq!(buffer.open(&EditorScript::fail(7)), 7);
    assert_eq!(buffer.text(), "same\n");
}

/// The script's directory is removed when it is dropped
#[test]
fn script_directory_is_removed_on_drop() {
    // note where the script lives, then drop it
    let script = EditorScript::noop();
    let path = script.script_path();
    assert!(path.exists());
    drop(script);
    assert!(!path.exists());
}

/// The recovery path is read from an error's text
#[test]
fn recovery_path_is_extracted() {
    assert_eq!(
        recovery_path("it failed (your edits were saved to '/tmp/x y/z.yml')"),
        Some(PathBuf::from("/tmp/x y/z.yml"))
    );
    assert_eq!(recovery_path("no file here"), None);
}

/// The editor command still runs while another process holds the script open for writing
#[test]
fn editor_runs_while_the_script_is_open_for_writing() {
    // hold the script open for writing, as a concurrently forked child would
    let script = EditorScript::noop();
    let _writer = std::fs::OpenOptions::new()
        .append(true)
        .open(script.script_path())
        .unwrap();
    let buffer = Buffer::new("same\n");
    // run the command the way the edit flow does
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{} '{}'", script.editor(), buffer.0.display()))
        .status()
        .unwrap();
    assert!(status.success(), "{status}");
}
