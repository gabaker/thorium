//! Tests that `thorctl completions` runs with no config, login, or API

use std::path::Path;
use std::process::{Command, Output};

/// Run thorctl in an empty home with no config and none of the thorctl environment
///
/// # Arguments
///
/// * `home` - The empty directory to use as HOME and the working directory
/// * `args` - The arguments to pass to thorctl
fn thorctl(home: &Path, args: &[&str]) -> Output {
    // run the real binary with nothing to load a config or keys from
    Command::new(env!("CARGO_BIN_EXE_thorctl"))
        .args(args)
        .current_dir(home)
        .env_clear()
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .output()
        .expect("failed to run thorctl")
}

/// A scratch home directory that is removed when it drops
struct Home(
    /// The path of the directory
    std::path::PathBuf,
);

impl Home {
    /// Create an empty home directory under the temp dir
    fn new() -> Self {
        // pick a unique path and create it
        let path =
            std::env::temp_dir().join(format!("thorctl-completions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("failed to create the home");
        Home(path)
    }
}

impl Drop for Home {
    /// Remove the home directory
    fn drop(&mut self) {
        // best effort cleanup of the scratch directory
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every shell prints a script with no config file present, and nothing is written to HOME
#[test]
fn completions_run_without_a_config() {
    // run every shell in an empty home
    let home = Home::new();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = thorctl(&home.0, &["completions", shell]);
        assert!(output.status.success(), "{shell} failed: {output:?}");
        let stdout = String::from_utf8(output.stdout).expect("script was not UTF-8");
        assert!(
            stdout.contains("thorctl"),
            "{shell} script never names thorctl"
        );
        assert!(output.stderr.is_empty(), "{shell} wrote to stderr");
    }
    // no config, keys, or cache was created
    let left: Vec<_> = std::fs::read_dir(&home.0)
        .expect("failed to read the home")
        .collect();
    assert!(left.is_empty(), "completions wrote files: {left:?}");
}

/// The bash script names real subcommands
#[test]
fn bash_completions_name_known_subcommands() {
    // generate the bash script in an empty home
    let home = Home::new();
    let output = thorctl(&home.0, &["completions", "bash"]);
    assert!(output.status.success(), "bash failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("script was not UTF-8");
    // each subcommand must appear
    for name in ["tools", "workflows", "runs", "files", "grep"] {
        assert!(stdout.contains(name), "bash script is missing {name}");
    }
}

/// An unknown shell is refused by the argument parser
#[test]
fn unknown_shell_is_refused() {
    // an unsupported shell name exits with a usage error and prints no script
    let home = Home::new();
    let output = thorctl(&home.0, &["completions", "tcsh"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "a refused shell printed a script");
}

/// The help names where to install the script for each shell
#[test]
fn help_explains_how_to_install() {
    // read the long help of the command
    let home = Home::new();
    let output = thorctl(&home.0, &["completions", "--help"]);
    assert!(output.status.success(), "help failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("help was not UTF-8");
    // every shell's install location must be spelled out
    for location in [
        "~/.local/share/bash-completion/completions/thorctl",
        "_thorctl",
        "$fpath",
        "~/.config/fish/completions/thorctl.fish",
        "$PROFILE",
    ] {
        assert!(stdout.contains(location), "help is missing {location}");
    }
}
