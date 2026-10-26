//! Tests that run `thorctl toolbox convert` as a program
//!
//! Converting is offline, so none of these tests start the test API, log in, or have a config
//! file; each runs with an empty home directory and a config path that does not exist.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The repository's legacy toolbox
fn legacy_toolbox() -> PathBuf {
    // the file CI builds the tool images from
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/toolbox.json")
}

/// The repository's converted toolbox
fn converted_toolbox() -> PathBuf {
    // committed beside the legacy one
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/toolbox.v2.json")
}

/// A small legacy toolbox with an image that cannot convert
fn small_toolbox() -> PathBuf {
    // test data owned by the conversion tests
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/toolbox/small.legacy.json")
}

/// A scratch directory removed when dropped
struct Scratch(PathBuf);

impl Scratch {
    /// Create an empty scratch directory with a unique name
    fn new() -> Self {
        // a random name keeps parallel tests apart
        let path =
            std::env::temp_dir().join(format!("thorctl-convert-bin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Scratch(path)
    }
}

impl Drop for Scratch {
    /// Remove the directory
    fn drop(&mut self) {
        // leftovers in the temp directory are harmless
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `thorctl toolbox convert` with no config and no network
///
/// # Arguments
///
/// * `home` - The empty home directory to run with
/// * `args` - The arguments after `convert`
fn convert(home: &Path, args: &[&str]) -> Output {
    // the config path does not exist, so any attempt to log in or connect would fail the test
    Command::new(env!("CARGO_BIN_EXE_thorctl"))
        .args([
            "--config",
            "/nonexistent/thorium.yml",
            "--skip-update",
            "toolbox",
            "convert",
        ])
        .args(args)
        .env("HOME", home)
        .env_remove("THORCTL__CLUSTERS")
        .output()
        .expect("run thorctl")
}

/// Get a path as an argument
///
/// # Arguments
///
/// * `path` - The path to display
fn arg(path: &Path) -> String {
    // paths in these tests are valid UTF-8
    path.display().to_string()
}

/// Converting the real toolbox with `--check` against the committed file passes
#[test]
fn the_committed_toolbox_is_current() {
    let scratch = Scratch::new();
    let output = convert(
        &scratch.0,
        &[
            &arg(&legacy_toolbox()),
            "--check",
            "-o",
            &arg(&converted_toolbox()),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"");
    // the report says what was converted
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("59 tools") && stderr.contains("32 pipelines"),
        "{stderr}"
    );
}

/// Printing the conversion gives the committed file byte for byte
#[test]
fn stdout_is_the_committed_toolbox() {
    let scratch = Scratch::new();
    let output = convert(&scratch.0, &[&arg(&legacy_toolbox()), "-o", "-"]);
    assert!(output.status.success());
    assert_eq!(output.stdout, std::fs::read(converted_toolbox()).unwrap());
}

/// A drifted output fails `--check` with exit code 1 and a diff
#[test]
fn a_drifted_output_fails_the_check() {
    let scratch = Scratch::new();
    let stale = scratch.0.join("stale.json");
    let current = std::fs::read_to_string(converted_toolbox()).unwrap();
    std::fs::write(&stale, current.replacen("clamav", "clamav-old", 1)).unwrap();
    let output = convert(
        &scratch.0,
        &[&arg(&legacy_toolbox()), "--check", "-o", &arg(&stale)],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("+++"));
}

/// `--strict` exits 1 on issues without writing, and the plain run exits 0 and writes
#[test]
fn strict_exits_one_on_issues() {
    let scratch = Scratch::new();
    let input = scratch.0.join("toolbox.json");
    std::fs::copy(small_toolbox(), &input).unwrap();
    let strict = convert(&scratch.0, &[&arg(&input), "--strict"]);
    assert_eq!(strict.status.code(), Some(1));
    assert!(!scratch.0.join("toolbox.v2.json").exists());
    assert!(String::from_utf8_lossy(&strict.stderr).contains("missing_field:image"));
    let plain = convert(&scratch.0, &[&arg(&input)]);
    assert_eq!(plain.status.code(), Some(0));
    assert!(scratch.0.join("toolbox.v2.json").exists());
}

/// Conflicting flags are usage errors, and `toolbox --help` lists the command
#[test]
fn usage_errors_and_help() {
    let scratch = Scratch::new();
    let output = convert(
        &scratch.0,
        &[&arg(&small_toolbox()), "--in-place", "-o", "out.json"],
    );
    assert_eq!(output.status.code(), Some(2));
    let help = Command::new(env!("CARGO_BIN_EXE_thorctl"))
        .args(["toolbox", "--help"])
        .output()
        .expect("run thorctl");
    assert!(String::from_utf8_lossy(&help.stdout).contains("convert"));
}
