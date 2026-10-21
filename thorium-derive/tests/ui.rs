//! Compile tests for the schema derives
//!
//! Each case in `tests/ui/fail` must fail to compile with every line of its `.stderr` file
//! somewhere in the compiler's output, and each case in `tests/ui/pass` must compile. The cases
//! are checked as binaries of a scratch crate outside the workspace that depends on
//! `thorium-api`, so they also prove the generated paths resolve from a downstream crate.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The features of `thorium-api` the scratch crate enables
const API_FEATURES: &str = r#"["scylla-utils", "client"]"#;

/// Get the root of this workspace
fn workspace_root() -> PathBuf {
    // this crate lives one directory below the workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("thorium-derive has a parent directory")
        .to_path_buf()
}

/// Write the scratch crate's manifest and lock file, returning its directory
///
/// # Arguments
///
/// * `cases` - The case files to add as binaries, with their names
fn write_scratch_crate(cases: &[(String, PathBuf)]) -> PathBuf {
    // keep the scratch crate in cargo's temporary directory for integration tests
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("thorium-derive-ui");
    let bin_dir = dir.join("src").join("bin");
    // start each run from an empty set of binaries so removed cases don't linger
    if bin_dir.exists() {
        fs::remove_dir_all(&bin_dir).expect("old scratch binaries are removed");
    }
    fs::create_dir_all(&bin_dir).expect("scratch binary directory is created");
    let root = workspace_root();
    // depend on the api crate by path, and on the crates the cases name directly
    let manifest = format!(
        "[package]\n\
         name = \"thorium-derive-ui\"\n\
         version = \"0.0.0\"\n\
         edition = \"2024\"\n\
         publish = false\n\n\
         [workspace]\n\n\
         [dependencies]\n\
         thorium-api = {{ path = {api:?}, default-features = false, features = {API_FEATURES} }}\n\
         chrono = \"0.4\"\n\
         uuid = \"1\"\n\
         serde = {{ version = \"1.0\", features = [\"derive\"] }}\n\
         serde_json = \"1.0\"\n",
        api = root.join("api").display().to_string(),
    );
    fs::write(dir.join("Cargo.toml"), manifest).expect("scratch manifest is written");
    // reuse the workspace's lock file so the cases build the same dependency versions
    fs::copy(root.join("Cargo.lock"), dir.join("Cargo.lock")).expect("lock file is copied");
    // copy each case in as its own binary
    for (name, path) in cases {
        fs::copy(path, bin_dir.join(format!("{name}.rs"))).expect("case is copied");
    }
    dir
}

/// Collect the cases in one directory, sorted by name
///
/// # Arguments
///
/// * `kind` - The case directory under `tests/ui` (`fail` or `pass`)
fn cases(kind: &str) -> Vec<(String, PathBuf)> {
    // read every Rust file in this case directory
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/ui")
        .join(kind);
    let mut found = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("{} can't be read: {err}", dir.display()))
        .map(|entry| entry.expect("case entry reads").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| {
            // name the binary after the case's file stem, prefixed by its kind
            let stem = path.file_stem().expect("case has a name").to_string_lossy();
            (format!("{kind}_{stem}"), path)
        })
        .collect::<Vec<_>>();
    found.sort();
    found
}

/// Check one case binary of the scratch crate, returning whether it compiled and its output
///
/// # Arguments
///
/// * `dir` - The scratch crate's directory
/// * `name` - The case binary to check
fn check(dir: &Path, name: &str) -> (bool, String) {
    // build the cases in their own target directory beside the scratch crate
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .env_remove("CARGO_MANIFEST_DIR")
        .args(["check", "--quiet", "--color", "never", "--bin", name])
        .output()
        .expect("cargo runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), stderr)
}

/// Every fail case fails with its expected messages and every pass case compiles
#[test]
fn schema_derive_cases() {
    // gather every case and lay out the scratch crate once
    let fail = cases("fail");
    let pass = cases("pass");
    let all = fail.iter().chain(pass.iter()).cloned().collect::<Vec<_>>();
    let dir = write_scratch_crate(&all);
    let mut problems = Vec::new();
    // each fail case must fail and print each expected line
    for (name, path) in &fail {
        let (compiled, stderr) = check(&dir, name);
        if compiled {
            problems.push(format!("{name} compiled but must fail"));
            continue;
        }
        let expected_path = path.with_extension("stderr");
        let expected = fs::read_to_string(&expected_path)
            .unwrap_or_else(|err| panic!("{} can't be read: {err}", expected_path.display()));
        for line in expected
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            if !stderr.contains(line) {
                problems.push(format!("{name} is missing `{line}` in:\n{stderr}"));
            }
        }
    }
    // each pass case must compile
    for (name, _) in &pass {
        let (compiled, stderr) = check(&dir, name);
        if !compiled {
            problems.push(format!("{name} must compile but failed:\n{stderr}"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
}
